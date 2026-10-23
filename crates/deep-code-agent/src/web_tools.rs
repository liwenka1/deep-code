//! Network tools: `web_search` and `fetch_url`.
//!
//! Both are served by DeepSeek's Search & Fetch API whenever the configured
//! provider endpoint is DeepSeek's own (`api.deepseek.com`): the key that
//! already pays for the model pays for search, fetch hands back a page as
//! Markdown, and nothing in this process parses HTML it did not ask for. On
//! any other endpoint `web_search` is still mounted but unavailable — its
//! description says so and a call returns that one error — while `fetch_url`
//! falls back to fetching from this machine behind the SSRF guard in [`ssrf`].
//! The backend is decided once, at mount time, by [`WebBackend::from_config`].
//!
//! The approval posture does not depend on the backend: both tools are
//! `ToolKind::Network` (approval required; `fetch_url` never takes a
//! session-wide consent), `[sandbox] network = "never"` refuses them, and
//! `DEEP_CODE_DISABLE_WEB` unmounts them.
//!
//! The two fetch paths are guarded differently, because the request leaves from
//! different places. The local path's guard resolves the host, refuses anything
//! that is not a public address, and pins every redirect hop to the address it
//! checked. The API path's request leaves DeepSeek's network, so a lookup that
//! fails here must not refuse a URL the service could fetch (with a proxy for
//! egress and no direct DNS, that refused every fetch while `web_search` kept
//! working); a name that *does* resolve to a private address here is still
//! refused, because sending a name from behind the user's firewall to a third
//! party leaks it. Literal private addresses and embedded credentials are
//! refused on both paths, and redirects are the service's to follow.
//!
//! Both wrap their blocking `reqwest` bodies in [`crate::tool::run_blocking`],
//! so requests land on the spawn_blocking pool.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::AgentConfig;
use crate::i18n::{Lang, SharedLang, TextId, tr, tr_with};
use crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
use crate::text_sanitize::collapse_whitespace;
use crate::text_util::truncate_chars;
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, run_blocking};

mod ssrf;

/// Raw download cap on the local fetch path (the API path has no body to cap:
/// the service returns Markdown, and [`render_fetched`] pages it).
const DEFAULT_MAX_FETCH_BYTES: usize = 512 * 1024;
/// One retry on the statuses the service documents as transient — 408, 429,
/// 5xx — because a failed Search/Fetch request is not billed and the model has
/// no way to know it could ask again.
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Longest wait honoured from a `Retry-After`. Past it the answer is the error
/// rather than the wait: a tool call may not sit for half a minute inside the
/// model's turn (and behind it, an approval).
const MAX_RETRY_WAIT: Duration = Duration::from_secs(2);
const DEFAULT_SEARCH_RESULTS: usize = 5;
const MAX_SEARCH_RESULTS: usize = 10;
/// The search service's own limit on `query`; checked here so the model gets
/// an argument error naming the limit instead of an opaque HTTP 400.
const MAX_QUERY_CHARS: usize = 128;
/// The fetch service's own limit on `url`.
const MAX_URL_CHARS: usize = 8192;
/// Per-result cap on the summary line: ten results must fit the output budget
/// with room to spare, and a summary is a pointer to a page, not the page.
const SUMMARY_CHARS: usize = 400;
const TITLE_CHARS: usize = 200;
/// Longest URL rendered per hit. A result URL is a link to click, not prose:
/// past a couple of hundred characters it is a redirect chain that crowds out
/// the summary the model actually reads.
const URL_CHARS: usize = 300;
/// Chars held back from [`TOOL_OUTPUT_BUDGET`] for the fetched page's header
/// and its continuation note, so a chunk never trips the runtime's head/tail
/// truncation — which would silently drop the MIDDLE of a page while the note
/// pointed past the end of what the model saw.
const FETCH_WRAPPER_RESERVE: usize = 600;
/// Floor on the chunk size regardless of header length (a maximal-length URL
/// is itself most of the budget), so every call still makes progress.
const MIN_FETCH_CHUNK_CHARS: usize = 1_000;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Longer than search: the service has to reach the page itself first.
const FETCH_TIMEOUT: Duration = Duration::from_secs(45);

const SEARCH_DESC: &str = "Search the web through DeepSeek's search service and return the top \
results (1-10, default 5): title, URL, date and a short summary. The query is at most 128 \
characters and supports operators such as site:, lang:, after: and before:. Queries are sent to \
DeepSeek's service.";
const SEARCH_DESC_UNAVAILABLE: &str = "NOT AVAILABLE on this endpoint: web search needs \
DeepSeek's API endpoint (api.deepseek.com) and an API key, so a call only returns that error. \
Use fetch_url for URLs you already know.";
const FETCH_DESC_API: &str = "Fetch a public http(s) URL through DeepSeek's fetch service and \
return the page as Markdown (title, then content). Long pages come back in chunks: when a result \
ends with a truncation note, call again with the `offset` it names to continue. The fetch — \
including any redirect it follows — happens on DeepSeek's network. A literal private/internal \
address, a host that resolves to one from here, and a URL with embedded credentials are refused.";
const FETCH_DESC_LOCAL: &str = "Fetch a public http(s) URL from this machine and return its \
text (HTML is reduced to readable text; the download is capped at 512 KiB). Long pages come back \
in chunks: when a result ends with a truncation note, call again with the `offset` it names to \
continue. Private/internal addresses and URLs with embedded credentials are refused.";

/// Where the web tools get their answers from, decided once from the provider
/// configuration when the registry is built.
#[derive(Debug, Clone)]
pub struct WebBackend {
    /// `Some` = DeepSeek's Search & Fetch API is reachable with the configured
    /// key; `None` = no search, and fetch runs locally.
    api: Option<ToolsApi>,
}

#[derive(Debug, Clone)]
struct ToolsApi {
    /// Base of the Search & Fetch API (`…/v1/tools`), no trailing slash.
    base: String,
    api_key: String,
}

impl WebBackend {
    /// The API backend when `provider.base_url` points at DeepSeek's own host
    /// AND a key is configured; local-only otherwise. A custom endpoint (a
    /// proxy, a compatible server) has no search service to call, and without
    /// a key the chat endpoint would not work either — but the fetch fallback
    /// still does, so the tools stay mounted.
    #[must_use]
    pub fn from_config(config: &AgentConfig) -> Self {
        let api = config.deepseek_tools_api_base().and_then(|base| {
            config.require_api_key().ok().map(|key| ToolsApi {
                base,
                api_key: key.to_string(),
            })
        });
        Self { api }
    }

    /// No search, local fetch only — what a non-DeepSeek endpoint gets, and
    /// what tests that exercise the local path start from.
    #[must_use]
    pub fn local_only() -> Self {
        Self { api: None }
    }

    /// Stable identifier for `doctor` and `/status`.
    #[must_use]
    pub fn id(&self) -> &'static str {
        if self.api.is_some() {
            "deepseek-tools-api"
        } else {
            "local-fetch-only"
        }
    }

    #[cfg(test)]
    fn test_api(base: &str) -> Self {
        Self {
            api: Some(ToolsApi {
                base: base.trim_end_matches('/').to_string(),
                api_key: "sk-test".to_string(),
            }),
        }
    }
}

/// Registry with both network tools, for [`crate::runtime_launch`] and the
/// sub-agent registry. The shared language handle keeps tool error text in
/// step with `/lang`.
#[must_use]
pub fn web_tool_registry(ui_lang: &SharedLang, backend: &WebBackend) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(WebSearchTool::new(ui_lang.clone(), backend));
    registry.register(FetchUrlTool::new(ui_lang.clone(), backend));
    registry
}

/// One call to the Search & Fetch API. Errors are already user-facing text.
///
/// No SSRF pin here: the destination is the configured provider host, trusted
/// exactly as much as the chat endpoint the same key is sent to.
///
/// Retried once on the statuses the service documents as transient (408, 429,
/// 5xx). Both calls are billed per *successful* request, so a retry after a
/// failure costs nothing but time, and the alternative is the model — which
/// has no idea it could retry — asking again, which also costs an approval.
fn api_post(
    api: &ToolsApi,
    path: &str,
    body: &Value,
    timeout: Duration,
    lang: Lang,
) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(ssrf::USER_AGENT)
        .build()
        .map_err(|error| {
            tr_with(
                lang,
                TextId::WebClientInitError,
                &[("error", &error.to_string())],
            )
        })?;
    let url = format!("{}/{path}", api.base);
    // What the service said the last time it failed *before* the retry. A
    // retry whose own connection cannot be made must report that first answer:
    // "HTTP 503: busy" is information, "error sending request" is not.
    let mut first_failure: Option<String> = None;
    let mut retried = false;
    let failure = |status: reqwest::StatusCode, text: &str| -> String {
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let message = api_error_message(text);
            if message.is_empty() {
                tr(lang, TextId::WebApiRateLimited).to_string()
            } else {
                // The service's own words: they name *which* limit (account QPS
                // or the per-domain fetch cap) and how long to wait.
                format!("{} {message}", tr(lang, TextId::WebApiRateLimited))
            }
        } else {
            tr_with(
                lang,
                TextId::WebApiHttpStatus,
                &[
                    ("status", &status.to_string()),
                    ("message", &api_error_message(text)),
                ],
            )
        }
    };
    loop {
        let response = match client
            .post(&url)
            .bearer_auth(&api.api_key)
            .json(body)
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                return Err(first_failure.unwrap_or_else(|| {
                    tr_with(
                        lang,
                        TextId::WebApiRequestError,
                        &[("error", &error.to_string())],
                    )
                }));
            }
        };
        let status = response.status();
        // Read before the body is consumed, and honour it when it is short: a
        // service asking for thirty seconds is not a stall this tool may take,
        // so past the cap the answer is the error, not the wait.
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = match response.text() {
            Ok(text) => text,
            Err(error) => {
                return Err(first_failure.unwrap_or_else(|| {
                    tr_with(
                        lang,
                        TextId::WebReadBodyError,
                        &[("error", &error.to_string())],
                    )
                }));
            }
        };
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|error| {
                tr_with(
                    lang,
                    TextId::WebApiBadResponse,
                    &[("error", &error.to_string())],
                )
            });
        }
        let transient = status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error();
        if transient && !retried {
            retried = true;
            first_failure = Some(failure(status, &text));
            let wait = retry_after.unwrap_or(RETRY_DELAY).min(MAX_RETRY_WAIT);
            // Blocking on purpose: this runs on the tool's own blocking worker.
            std::thread::sleep(wait);
            continue;
        }
        return Err(failure(status, &text));
    }
}

/// The `error.message` of an OpenAI-shaped error body, else the body itself;
/// flattened and capped because it is quoted into a one-line tool error.
fn api_error_message(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")?
                .get("message")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string());
    truncate_chars(&collapse_whitespace(&message), 200)
}

#[derive(Debug, Clone)]
pub struct FetchUrlTool {
    api: Option<ToolsApi>,
    /// Test hook: lift the public-address requirement so unit tests can hit
    /// a local server. Never set in production registries.
    allow_private: bool,
    max_bytes: usize,
    description: &'static str,
    lang: SharedLang,
    /// Pages already fetched this session. Shared by clones of the tool (the
    /// registry hands out one clone per call), dropped with the tool when the
    /// runtime is relaunched.
    cache: Arc<Mutex<FetchCache>>,
}

/// How many pages are kept for continuation. A session can fetch for hours;
/// the pages are bounded by count here and by [`DEFAULT_MAX_FETCH_BYTES`] each,
/// so the worst case is a few megabytes rather than the whole session.
const CACHED_PAGES: usize = 8;

/// One page as this session rendered it: title, text, and the note about the
/// download having been cut. Stored post-render because that is what the
/// `offset` a continuation names indexes into.
#[derive(Debug, Clone)]
struct CachedPage {
    url: String,
    title: String,
    content: String,
    note: Option<String>,
}

/// Why this exists: `offset` is a character index into the rendering, so a
/// continuation has to page through *the same* text. Refetching both re-bills
/// the request (Search and Fetch are billed per successful call) and lets the
/// boundaries drift if the page changed in between — the chunk the model was
/// told to read next may then be different text.
///
/// Only continuations read it. `offset == 0` always refetches and overwrites the
/// entry, so "fetch this URL again" keeps meaning "get me the page as it is
/// now".
#[derive(Debug, Default)]
struct FetchCache {
    pages: VecDeque<CachedPage>,
}

impl FetchCache {
    fn get(&mut self, url: &str) -> Option<CachedPage> {
        let index = self.pages.iter().position(|page| page.url == url)?;
        let page = self.pages.remove(index)?;
        // Most recently used last: that is the end the cap drops from.
        self.pages.push_back(page.clone());
        Some(page)
    }

    fn put(&mut self, page: CachedPage) {
        self.pages.retain(|old| old.url != page.url);
        self.pages.push_back(page);
        while self.pages.len() > CACHED_PAGES {
            self.pages.pop_front();
        }
    }
}

impl FetchUrlTool {
    pub const NAME: &'static str = "fetch_url";

    #[must_use]
    pub fn new(lang: SharedLang, backend: &WebBackend) -> Self {
        let description = if backend.api.is_some() {
            FETCH_DESC_API
        } else {
            FETCH_DESC_LOCAL
        };
        Self {
            api: backend.api.clone(),
            allow_private: false,
            max_bytes: DEFAULT_MAX_FETCH_BYTES,
            description,
            lang,
            cache: Arc::new(Mutex::new(FetchCache::default())),
        }
    }

    fn fetch_sync(&self, params: FetchUrlParams) -> Result<ToolOutput, ToolError> {
        let lang = self.lang.get();
        let parsed = match Url::parse(&params.url) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(ToolOutput::soft_error(tr_with(
                    lang,
                    TextId::WebUrlParseError,
                    &[("error", &error.to_string())],
                )));
            }
        };
        // Embedded credentials are refused on both paths: the local client
        // would send them as basic auth, and the service rejects them — and a
        // model has no business carrying a password in a URL either way.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Ok(ToolOutput::soft_error(
                tr(lang, TextId::WebUrlHasCredentials).to_string(),
            ));
        }
        // Which guard applies depends on who sends the request. The local path's
        // request leaves this machine, so its host must resolve to a public
        // address and the connection is pinned to it (`pinned_get` repeats that
        // per redirect hop). The API path's request leaves DeepSeek's, so a
        // lookup that fails here must not refuse a URL the service can reach —
        // but a name this machine *can* see pointing inside the user's network
        // is still refused, because handing it to a third party leaks it.
        let guard = if self.api.is_some() {
            ssrf::check_api_url(&parsed, self.allow_private, lang)
        } else {
            ssrf::check_url(&parsed, self.allow_private, lang).map(|_| ())
        };
        if let Err(reason) = guard {
            return Ok(ToolOutput::soft_error(reason));
        }
        // Length is a property of the argument, not of the backend: the service
        // documents 8192 characters and the local reader has no business
        // accepting a longer one either (a URL that long leaves no room for the
        // page in the tool's output budget).
        if params.url.chars().count() > MAX_URL_CHARS {
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::WebUrlTooLong,
                &[("max", &MAX_URL_CHARS.to_string())],
            )));
        }
        let offset = params.offset.unwrap_or(0);

        // A continuation pages through the text this session already fetched —
        // see `FetchCache`. `offset == 0` always refetches, so "fetch it again"
        // keeps meaning "get me the page as it is now".
        let cached = if offset > 0 {
            self.cache
                .lock()
                .ok()
                .and_then(|mut cache| cache.get(parsed.as_str()))
        } else {
            None
        };
        let page = match cached {
            Some(page) => page,
            None => {
                let page = match self.fetch_page(&parsed, lang) {
                    Ok(page) => page,
                    Err(output) => return Ok(output),
                };
                if let Ok(mut cache) = self.cache.lock() {
                    cache.put(page.clone());
                }
                page
            }
        };

        match render_fetched(
            &parsed,
            &page.title,
            &page.content,
            page.note.as_deref(),
            offset,
            lang,
        ) {
            Ok(text) => Ok(ToolOutput::text(text)),
            Err(reason) => Ok(ToolOutput::soft_error(reason)),
        }
    }

    /// One fresh fetch: whichever path the backend chose, rendered into the
    /// page the continuation cache stores. `Err` is the tool output for a
    /// refusal (a soft error), already localized.
    fn fetch_page(&self, parsed: &Url, lang: Lang) -> Result<CachedPage, ToolOutput> {
        let (title, content, note) = match &self.api {
            Some(api) => {
                let response = match api_post(
                    api,
                    "fetch",
                    &json!({ "url": parsed.as_str() }),
                    FETCH_TIMEOUT,
                    lang,
                ) {
                    Ok(response) => response,
                    Err(reason) => return Err(ToolOutput::soft_error(reason)),
                };
                let Some(content) = response.get("content").and_then(Value::as_str) else {
                    return Err(ToolOutput::soft_error(tr_with(
                        lang,
                        TextId::WebApiBadResponse,
                        &[("error", "missing `content`")],
                    )));
                };
                let title = response
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (title, content.to_string(), None)
            }
            None => {
                let response = match ssrf::pinned_get(parsed.clone(), self.allow_private, lang) {
                    Ok(response) => response,
                    Err(reason) => return Err(ToolOutput::soft_error(reason)),
                };
                let status = response.status();
                if !status.is_success() {
                    return Err(ToolOutput::soft_error(tr_with(
                        lang,
                        TextId::WebHttpStatus,
                        &[("status", &status.to_string())],
                    )));
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let mut body = Vec::new();
                let mut limited = response.take(self.max_bytes as u64 + 1);
                if let Err(error) = limited.read_to_end(&mut body) {
                    return Err(ToolOutput::soft_error(tr_with(
                        lang,
                        TextId::WebReadBodyError,
                        &[("error", &error.to_string())],
                    )));
                }
                let capped = body.len() > self.max_bytes;
                body.truncate(self.max_bytes);
                let raw = String::from_utf8_lossy(&body);
                let text = if content_type.contains("text/html") {
                    html_to_text(&raw)
                } else {
                    raw.to_string()
                };
                (
                    String::new(),
                    text,
                    capped.then(|| {
                        tr_with(
                            lang,
                            TextId::WebContentTruncated,
                            &[("max", &human_bytes(self.max_bytes))],
                        )
                    }),
                )
            }
        };
        Ok(CachedPage {
            url: parsed.as_str().to_string(),
            title,
            content,
            note,
        })
    }
}

/// Header + one chunk of `content` starting at char `offset`, sized to the
/// runtime's output budget, with a continuation note naming the next offset
/// when more remains. Chunks tile the content exactly: the note's
/// `next_offset` is the first char not shown, so consecutive calls neither
/// overlap nor skip. `Err` is the user-facing text for an offset past the end.
fn render_fetched(
    url: &Url,
    title: &str,
    content: &str,
    source_note: Option<&str>,
    offset: usize,
    lang: Lang,
) -> Result<String, String> {
    let total = content.chars().count();
    if offset > total {
        return Err(tr_with(
            lang,
            TextId::WebFetchOffsetBeyondEnd,
            &[
                ("offset", &offset.to_string()),
                ("total", &total.to_string()),
            ],
        ));
    }
    let mut header = String::new();
    let title = truncate_chars(&collapse_whitespace(title), TITLE_CHARS);
    if !title.is_empty() {
        header.push_str(&title);
        header.push('\n');
    }
    header.push_str(url.as_str());
    header.push('\n');
    if let Some(note) = source_note {
        header.push_str(note);
        header.push('\n');
    }
    header.push('\n');
    let budget = TOOL_OUTPUT_BUDGET
        .saturating_sub(FETCH_WRAPPER_RESERVE)
        .saturating_sub(header.chars().count())
        .max(MIN_FETCH_CHUNK_CHARS);
    let end = offset.saturating_add(budget).min(total);
    let chunk: String = content.chars().skip(offset).take(end - offset).collect();
    let mut out = header;
    out.push_str(&chunk);
    if end < total {
        out.push_str("\n\n");
        out.push_str(&tr_with(
            lang,
            TextId::WebFetchContinue,
            &[
                ("start", &offset.to_string()),
                ("end", &end.to_string()),
                ("total", &total.to_string()),
                ("next_offset", &end.to_string()),
            ],
        ));
    } else if offset > 0 {
        out.push_str("\n\n");
        out.push_str(tr(lang, TextId::WebFetchEnd));
    }
    Ok(out)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchUrlParams {
    /// Absolute http or https URL to fetch.
    url: String,
    /// Character offset to continue a long page from — the value a previous
    /// result's truncation note named. Omit to start from the beginning.
    offset: Option<usize>,
}

#[async_trait]
impl Tool for FetchUrlTool {
    type Params = FetchUrlParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        self.description
    }

    async fn run(&self, params: FetchUrlParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.fetch_sync(params)).await
    }
}

#[derive(Debug, Clone)]
pub struct WebSearchTool {
    api: Option<ToolsApi>,
    description: &'static str,
    lang: SharedLang,
}

impl WebSearchTool {
    pub const NAME: &'static str = "web_search";

    #[must_use]
    pub fn new(lang: SharedLang, backend: &WebBackend) -> Self {
        let description = if backend.api.is_some() {
            SEARCH_DESC
        } else {
            SEARCH_DESC_UNAVAILABLE
        };
        Self {
            api: backend.api.clone(),
            description,
            lang,
        }
    }

    fn search_sync(&self, params: WebSearchParams) -> Result<ToolOutput, ToolError> {
        let lang = self.lang.get();
        let query = params.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArguments {
                name: Self::NAME.to_string(),
                message: "missing string field 'query'".to_string(),
            });
        }
        let query_chars = query.chars().count();
        if query_chars > MAX_QUERY_CHARS {
            return Err(ToolError::InvalidArguments {
                name: Self::NAME.to_string(),
                message: format!(
                    "query must be at most {MAX_QUERY_CHARS} characters (got {query_chars})"
                ),
            });
        }
        let max_results = params.max_results.map_or(DEFAULT_SEARCH_RESULTS, |value| {
            value.clamp(1, MAX_SEARCH_RESULTS)
        });
        let Some(api) = &self.api else {
            return Ok(ToolOutput::soft_error(
                tr(lang, TextId::WebSearchUnavailable).to_string(),
            ));
        };

        let body = json!({ "query": query, "count": max_results });
        let response = match api_post(api, "search", &body, SEARCH_TIMEOUT, lang) {
            Ok(response) => response,
            Err(reason) => return Ok(ToolOutput::soft_error(reason)),
        };
        let Some(hits) = parse_search_hits(&response) else {
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::WebApiBadResponse,
                &[("error", "missing `results` array")],
            )));
        };
        if hits.is_empty() {
            return Ok(ToolOutput::soft_error(
                tr(lang, TextId::WebSearchNoResults).to_string(),
            ));
        }
        // The service is asked for `count` and normally honours it, but a
        // result set larger than the output budget would have its middle cut
        // by the runtime's head/tail trim — silently dropping whole hits while
        // the status still read Success.
        let hits: Vec<SearchHit> = hits.into_iter().take(max_results).collect();
        Ok(ToolOutput::text(render_search_hits(&hits)))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchParams {
    /// Search query (at most 128 characters).
    query: String,
    /// Number of results to return (1-10, default 5).
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for WebSearchTool {
    type Params = WebSearchParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        self.description
    }

    async fn run(&self, params: WebSearchParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.search_sync(params)).await
    }
}

/// One search result, read leniently: the service documents several nullable
/// fields, and a missing one is an empty string here, never a failed call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
    /// The longer, model-oriented blurb; preferred over `snippet` when present.
    summary: String,
    date: String,
    site_name: String,
}

/// `None` when the body has no `results` array at all — the one shape error
/// worth distinguishing from "no results".
fn parse_search_hits(response: &Value) -> Option<Vec<SearchHit>> {
    let results = response.get("results")?.as_array()?;
    let text = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(collapse_whitespace)
            .unwrap_or_default()
    };
    Some(
        results
            .iter()
            .map(|item| SearchHit {
                title: text(item, "title"),
                url: text(item, "url"),
                snippet: text(item, "snippet"),
                summary: text(item, "summary"),
                date: text(item, "date"),
                site_name: text(item, "site_name"),
            })
            .collect(),
    )
}

fn render_search_hits(hits: &[SearchHit]) -> String {
    let mut out = String::new();
    for (index, hit) in hits.iter().enumerate() {
        let title = if hit.title.is_empty() {
            hit.url.as_str()
        } else {
            hit.title.as_str()
        };
        out.push_str(&format!(
            "{}. {}\n   {}\n",
            index + 1,
            truncate_chars(title, TITLE_CHARS),
            truncate_chars(hit.url.as_str(), URL_CHARS)
        ));
        let meta: Vec<&str> = [hit.date.as_str(), hit.site_name.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        if !meta.is_empty() {
            out.push_str(&format!("   {}\n", meta.join(" · ")));
        }
        let blurb = if hit.summary.is_empty() {
            &hit.snippet
        } else {
            &hit.summary
        };
        if !blurb.is_empty() {
            out.push_str(&format!("   {}\n", truncate_chars(blurb, SUMMARY_CHARS)));
        }
    }
    out
}

/// A download cap for a human: `512 KiB`, or `16 bytes` when the cap is too
/// small to round to a whole KiB. The note used to spell "512 KiB" as a
/// constant while the actual limit was a field, so the text claimed a cap the
/// code was not applying.
fn human_bytes(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

/// Crude but dependency-free HTML → text for the local fetch path: drop
/// script/style, strip tags, decode common entities, collapse blank lines.
fn html_to_text(html: &str) -> String {
    static SCRIPT_RE: OnceLock<Regex> = OnceLock::new();
    static STYLE_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let script_re =
        SCRIPT_RE.get_or_init(|| Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap());
    let style_re = STYLE_RE.get_or_init(|| Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap());
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"(?s)<[^>]*>").unwrap());

    let stripped = script_re.replace_all(html, " ");
    let stripped = style_re.replace_all(&stripped, " ");
    let stripped = tag_re.replace_all(&stripped, " ");
    let decoded = decode_entities(&stripped);

    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let line = collapse_whitespace(line);
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;

    use serde_json::json;

    use crate::tool::{ErasedTool, ToolCall, ToolCx, ToolResultStatus};

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new("call_1", name, arguments)
    }

    fn zh() -> SharedLang {
        SharedLang::new(Lang::Zh)
    }

    fn local_fetch_tool() -> FetchUrlTool {
        FetchUrlTool {
            allow_private: true,
            ..FetchUrlTool::new(zh(), &WebBackend::local_only())
        }
    }

    fn serve_once(body: String, content_type: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    /// What a mock tools-API server saw of the one request it answered.
    struct Captured {
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    /// Read one HTTP request off `stream` and hand back `(head, body)`.
    fn read_http_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                return (String::new(), Vec::new());
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..split]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let body_start = split + 4;
            if buffer.len() < body_start + length {
                continue;
            }
            return (head, buffer[body_start..body_start + length].to_vec());
        }
    }

    /// A one-shot tools-API server: answers with `status` + `body` and hands
    /// the request it received back through the channel. Returns the base to
    /// give [`WebBackend::test_api`].
    fn serve_api_once(status: u16, body: String) -> (String, mpsc::Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let (head, request_body) = read_http_request(&mut stream);
            let header = |name: &str| {
                head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_string())
                })
            };
            let path = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let _ = tx.send(Captured {
                path,
                authorization: header("authorization"),
                body: serde_json::from_slice(&request_body).unwrap_or(Value::Null),
            });
            let response = format!(
                "HTTP/1.1 {status} Whatever\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    /// A tools-API server that answers `responses` in order, one per
    /// connection — what a retry needs: one endpoint, two different answers.
    fn serve_api_responses(responses: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = read_http_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 {status} Whatever\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    async fn run(tool: &dyn ErasedTool, name: &str, arguments: Value) -> crate::tool::ToolResult {
        tool.execute(&call(name, arguments), &ToolCx::default())
            .await
            .unwrap()
    }

    fn config(base_url: &str, api_key: Option<&str>) -> AgentConfig {
        AgentConfig {
            base_url: base_url.to_string(),
            api_key: api_key.map(str::to_string),
            ..AgentConfig::builtin()
        }
    }

    /// The API backend is chosen by host, not by path: the default `/beta`
    /// endpoint and a plain `/v1` both qualify, any other host (a proxy, a
    /// compatible server) does not, plaintext does not, and a key is required.
    #[test]
    fn the_backend_follows_the_provider_endpoint_and_key() {
        for base in [
            crate::config::DEFAULT_DEEPSEEK_BASE_URL,
            "https://api.deepseek.com/v1/",
            "https://API.DeepSeek.com",
        ] {
            assert_eq!(
                WebBackend::from_config(&config(base, Some("sk-x"))).id(),
                "deepseek-tools-api",
                "{base}"
            );
        }
        for base in [
            "https://proxy.example.com/v1",
            "https://api.deepseek.com.evil.example/v1",
            "http://api.deepseek.com/beta",
            "not a url",
        ] {
            assert_eq!(
                WebBackend::from_config(&config(base, Some("sk-x"))).id(),
                "local-fetch-only",
                "{base}"
            );
        }
        for key in [None, Some(""), Some("   ")] {
            assert_eq!(
                WebBackend::from_config(&config(crate::config::DEFAULT_DEEPSEEK_BASE_URL, key))
                    .id(),
                "local-fetch-only",
                "{key:?}"
            );
        }
    }

    #[tokio::test]
    async fn web_search_sends_the_query_to_the_tools_api_and_renders_hits() {
        let (base, seen) = serve_api_once(
            200,
            json!({
                "results": [
                    {
                        "title": "  Example   Docs ",
                        "url": "https://example.com/docs",
                        "snippet": "short",
                        "summary": "The   long summary for models.",
                        "date": "2026-10-01",
                        "site_name": "Example",
                        "authority": 3,
                        "rerank_score": 0.9
                    },
                    {
                        "title": "",
                        "url": "https://example.org/",
                        "snippet": "only a snippet",
                        "summary": "",
                        "date": null,
                        "site_name": null
                    }
                ]
            })
            .to_string(),
        );
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            WebSearchTool::NAME,
            json!({ "query": "  example docs ", "max_results": 2 }),
        )
        .await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert_eq!(
            result.content,
            "1. Example Docs\n   https://example.com/docs\n   2026-10-01 · Example\n   The long summary for models.\n\
             2. https://example.org/\n   https://example.org/\n   only a snippet\n"
        );

        let request = seen.recv().unwrap();
        assert_eq!(request.path, "/search");
        assert_eq!(request.authorization.as_deref(), Some("Bearer sk-test"));
        assert_eq!(request.body, json!({ "query": "example docs", "count": 2 }));
    }

    #[tokio::test]
    async fn web_search_caps_each_summary() {
        let long = "字".repeat(SUMMARY_CHARS + 50);
        let (base, _seen) = serve_api_once(
            200,
            json!({ "results": [{ "title": "t", "url": "https://e.com/", "summary": long }] })
                .to_string(),
        );
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(&tool, WebSearchTool::NAME, json!({ "query": "q" })).await;
        let blurb = result.content.lines().nth(2).unwrap().trim();
        assert_eq!(blurb.chars().count(), SUMMARY_CHARS + 1, "{blurb}");
        assert!(blurb.ends_with('…'));
    }

    /// The count is enforced here rather than trusted to the service: a body
    /// with more hits than were asked for used to be rendered whole, and past
    /// the output budget the runtime's head/tail trim dropped whole hits from
    /// the middle while the status still read Success.
    #[tokio::test]
    async fn web_search_renders_no_more_hits_than_were_asked_for() {
        let results: Vec<serde_json::Value> = (0..5)
            .map(|index| {
                json!({
                    "title": format!("hit {index}"),
                    "url": format!("https://e.com/{index}"),
                    "summary": "s",
                })
            })
            .collect();
        let (base, _seen) = serve_api_once(200, json!({ "results": results }).to_string());
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            WebSearchTool::NAME,
            json!({ "query": "q", "max_results": 2 }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Success);
        assert!(result.content.contains("1. hit 0"), "{}", result.content);
        assert!(result.content.contains("2. hit 1"), "{}", result.content);
        assert!(
            !result.content.contains("hit 2"),
            "a third hit was neither asked for nor wanted: {}",
            result.content
        );
    }

    /// A result URL is capped on its own: an unbounded URL line was the one
    /// field that could still push a page of hits past the output budget.
    #[tokio::test]
    async fn web_search_caps_each_url() {
        let long = format!("https://e.com/{}", "a".repeat(URL_CHARS + 50));
        let (base, _seen) = serve_api_once(
            200,
            json!({ "results": [{ "title": "t", "url": long, "summary": "s" }] }).to_string(),
        );
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(&tool, WebSearchTool::NAME, json!({ "query": "q" })).await;
        let url_line = result.content.lines().nth(1).unwrap().trim();
        assert_eq!(url_line.chars().count(), URL_CHARS + 1, "{url_line}");
        assert!(url_line.ends_with('…'));
    }

    /// A continuation pages the text this session already fetched: the mock
    /// answers exactly one request, so a second fetch could only fail. That is
    /// also the money: Search and Fetch bill per successful request, and a
    /// page read in five chunks used to be paid for five times. And the
    /// boundaries stay put — `offset` indexes the rendering the model was told
    /// to continue, not a fresh one.
    #[tokio::test]
    async fn fetch_url_continues_from_the_cached_page_without_asking_again() {
        let content = "abcdefghij".repeat(20);
        let (base, _seen) =
            serve_api_once(200, json!({ "title": "t", "content": content }).to_string());
        let tool = FetchUrlTool {
            allow_private: true,
            ..FetchUrlTool::new(zh(), &WebBackend::test_api(&base))
        };
        let url = format!("{base}/page");
        let first = run(&tool, FetchUrlTool::NAME, json!({ "url": url })).await;
        assert_eq!(first.status, ToolResultStatus::Success, "{}", first.content);

        let second = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": url, "offset": 6 }),
        )
        .await;
        assert_eq!(
            second.status,
            ToolResultStatus::Success,
            "{}",
            second.content
        );
        assert!(
            second.content.contains(&content[6..12]),
            "the continuation starts where the note said: {}",
            second.content
        );
    }

    /// `offset == 0` is a fresh fetch by definition: asking for a URL again
    /// must not answer with what the cache still has.
    #[tokio::test]
    async fn fetch_url_refetches_from_the_top() {
        let (base, _seen) = serve_api_once(
            200,
            json!({ "title": "t", "content": "abcdefghij" }).to_string(),
        );
        let tool = FetchUrlTool {
            allow_private: true,
            ..FetchUrlTool::new(zh(), &WebBackend::test_api(&base))
        };
        let url = format!("{base}/page");
        let first = run(&tool, FetchUrlTool::NAME, json!({ "url": url })).await;
        assert_eq!(first.status, ToolResultStatus::Success, "{}", first.content);
        // The mock is gone: only a real refetch can answer this, and that is the
        // point — a cached page would have come back as Success.
        let again = run(&tool, FetchUrlTool::NAME, json!({ "url": url })).await;
        assert_eq!(again.status, ToolResultStatus::Error, "{}", again.content);
    }

    /// A 5xx is the service's own "try again later": one bounded retry absorbs
    /// a blip instead of costing the model a turn and the user an approval.
    #[tokio::test]
    async fn a_transient_failure_is_retried_once() {
        let base = serve_api_responses(vec![
            (503, json!({ "error": { "message": "busy" } }).to_string()),
            (
                200,
                json!({ "results": [{ "title": "t", "url": "https://e.com/", "summary": "s" }] })
                    .to_string(),
            ),
        ]);
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(&tool, WebSearchTool::NAME, json!({ "query": "q" })).await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
    }

    /// A 429 that survives the retry keeps the service's own words: they name
    /// which limit was hit and how long to wait, and the fixed sentence alone
    /// threw that away.
    #[tokio::test]
    async fn a_rate_limit_that_survives_the_retry_keeps_the_service_message() {
        let base = serve_api_responses(vec![
            (
                429,
                json!({ "error": { "message": "account QPS exceeded" } }).to_string(),
            ),
            (
                429,
                json!({ "error": { "message": "per-domain fetch limit: wait 2s" } }).to_string(),
            ),
        ]);
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(&tool, WebSearchTool::NAME, json!({ "query": "q" })).await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("per-domain fetch limit"),
            "the service's message has to survive: {}",
            result.content
        );
    }

    /// No API means no search — the tool says so in its description (so the
    /// model can plan around it) and in its result, and nothing is scraped.
    #[tokio::test]
    async fn web_search_is_unavailable_without_the_tools_api() {
        let tool = WebSearchTool::new(zh(), &WebBackend::local_only());
        assert!(tool.description().starts_with("NOT AVAILABLE"));
        let result = run(&tool, WebSearchTool::NAME, json!({ "query": "anything" })).await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("api.deepseek.com"),
            "{}",
            result.content
        );

        let available = WebSearchTool::new(zh(), &WebBackend::test_api("http://127.0.0.1:1"));
        assert!(available.description().starts_with("Search the web"));
    }

    #[tokio::test]
    async fn web_search_reports_empty_results_rate_limits_and_bad_bodies() {
        let (base, _) = serve_api_once(200, json!({ "results": [] }).to_string());
        let result = run(
            &WebSearchTool::new(zh(), &WebBackend::test_api(&base)),
            WebSearchTool::NAME,
            json!({ "query": "q" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("没有搜索结果"),
            "{}",
            result.content
        );

        let (base, _) = serve_api_once(
            429,
            json!({ "error": { "message": "slow down", "type": "rate_limit", "param": null } })
                .to_string(),
        );
        let result = run(
            &WebSearchTool::new(zh(), &WebBackend::test_api(&base)),
            WebSearchTool::NAME,
            json!({ "query": "q" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("429"), "{}", result.content);

        let (base, _) = serve_api_once(
            400,
            json!({ "error": { "message": "query too long", "type": "invalid_request_error", "param": null } })
                .to_string(),
        );
        let result = run(
            &WebSearchTool::new(zh(), &WebBackend::test_api(&base)),
            WebSearchTool::NAME,
            json!({ "query": "q" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("400") && result.content.contains("query too long"),
            "{}",
            result.content
        );

        let (base, _) = serve_api_once(200, json!({ "unexpected": true }).to_string());
        let result = run(
            &WebSearchTool::new(zh(), &WebBackend::test_api(&base)),
            WebSearchTool::NAME,
            json!({ "query": "q" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("results"), "{}", result.content);
    }

    #[tokio::test]
    async fn web_search_rejects_blank_and_overlong_queries_before_any_request() {
        // An unreachable base: a request would surface as a connection error,
        // not as the argument error asserted here.
        let tool = WebSearchTool::new(zh(), &WebBackend::test_api("http://127.0.0.1:1"));
        for query in [" ".to_string(), "x".repeat(MAX_QUERY_CHARS + 1)] {
            let error = tool
                .execute(
                    &call(WebSearchTool::NAME, json!({ "query": query })),
                    &ToolCx::default(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{query:?}"
            );
        }
    }

    /// The API path returns the page in chunks that tile the content exactly:
    /// following every `next_offset` reproduces the whole page once, the last
    /// chunk is marked as the end, and an offset past the end is refused.
    #[tokio::test]
    async fn fetch_url_via_the_tools_api_pages_the_markdown() {
        let content: String = (0..30_000)
            .map(|i| char::from_u32(0x4e00 + (i % 500) as u32).unwrap())
            .collect();
        let total = content.chars().count();
        let mut reassembled = String::new();
        let mut offset = 0usize;
        let mut chunks = 0usize;
        loop {
            let (base, seen) = serve_api_once(
                200,
                json!({ "title": "Example  article", "content": content }).to_string(),
            );
            let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
            let mut arguments = json!({ "url": "https://1.1.1.1/article" });
            if offset > 0 {
                arguments["offset"] = json!(offset);
            }
            let result = run(&tool, FetchUrlTool::NAME, arguments).await;
            assert_eq!(
                result.status,
                ToolResultStatus::Success,
                "{}",
                result.content
            );
            assert!(result.content.chars().count() <= TOOL_OUTPUT_BUDGET);
            let request = seen.recv().unwrap();
            assert_eq!(request.path, "/fetch");
            assert_eq!(request.body, json!({ "url": "https://1.1.1.1/article" }));

            let (header, body) = result.content.split_once("\n\n").unwrap();
            assert_eq!(header, "Example article\nhttps://1.1.1.1/article");
            chunks += 1;
            if let Some((chunk, note)) = body.rsplit_once("\n\n[") {
                let note = format!("[{note}");
                if let Some(next) = note
                    .split("offset=")
                    .nth(1)
                    .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|digits| digits.parse::<usize>().ok())
                {
                    assert!(note.contains(&format!("共 {total}")), "{note}");
                    reassembled.push_str(chunk);
                    offset = next;
                    continue;
                }
                assert!(note.contains("内容结束"), "{note}");
                reassembled.push_str(chunk);
                break;
            }
            panic!("every chunk of a multi-chunk page carries a note: {body:?}");
        }
        assert!(
            chunks >= 3,
            "30k chars must need several chunks, got {chunks}"
        );
        assert_eq!(reassembled, content, "chunks must tile the page exactly");

        let (base, _) = serve_api_once(200, json!({ "title": "", "content": "abc" }).to_string());
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://1.1.1.1/", "offset": 4 }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("offset 4"), "{}", result.content);
    }

    /// A short page is returned whole with no note at all, and an untitled
    /// page gets no empty title line.
    #[tokio::test]
    async fn fetch_url_via_the_tools_api_returns_a_short_page_whole() {
        let (base, _) = serve_api_once(
            200,
            json!({ "title": "", "content": "# Hi\n\nbody" }).to_string(),
        );
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://1.1.1.1/x" }),
        )
        .await;
        assert_eq!(result.content, "https://1.1.1.1/x\n\n# Hi\n\nbody");
        assert!(tool.description().contains("Markdown"));
    }

    /// The SSRF guard and the credential check run before any request leaves
    /// the process on the API path too: the mock API never sees a connection.
    #[tokio::test]
    async fn fetch_url_refuses_private_targets_and_credentials_before_calling_the_api() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        for (url, expect) in [
            ("http://127.0.0.1:9/", "非公网"),
            ("http://169.254.169.254/latest/meta-data/", "非公网"),
            ("https://user:pw@1.1.1.1/", "用户名或密码"),
            ("file:///etc/passwd", "file"),
        ] {
            let result = run(&tool, FetchUrlTool::NAME, json!({ "url": url })).await;
            assert_eq!(result.status, ToolResultStatus::Error, "{url}");
            assert!(result.content.contains(expect), "{url}: {}", result.content);
        }
        let long = format!("https://1.1.1.1/{}", "a".repeat(MAX_URL_CHARS));
        let result = run(&tool, FetchUrlTool::NAME, json!({ "url": long })).await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("8192"), "{}", result.content);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "no refused URL may reach the API"
        );
    }

    #[tokio::test]
    async fn fetch_url_api_errors_are_soft_errors() {
        let (base, _) = serve_api_once(
            502,
            json!({ "error": { "message": "upstream unreachable", "type": "server_error", "param": null } })
                .to_string(),
        );
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://1.1.1.1/" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("upstream unreachable"),
            "{}",
            result.content
        );

        let (base, _) = serve_api_once(200, json!({ "title": "no content key" }).to_string());
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://1.1.1.1/" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("content"), "{}", result.content);
    }

    #[test]
    fn html_to_text_strips_scripts_and_decodes_entities() {
        let html = "<html><head><style>body{}</style><script>alert(1)</script></head>\
                    <body><h1>Hello &amp; 你好</h1>\n\n\n<p>a &lt;b&gt;</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello & 你好"));
        assert!(text.contains("a <b>"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("body{}"));
    }

    /// The URL length limit is a property of the argument, not of the backend:
    /// only the API path used to check it, so on a non-DeepSeek endpoint a
    /// 9000-character URL went out — and a URL that long leaves no room for the
    /// page in the tool's output budget.
    #[tokio::test]
    async fn a_url_past_the_limit_is_refused_on_the_local_path_too() {
        let tool = local_fetch_tool();
        let long = format!("http://127.0.0.1:9/{}", "a".repeat(MAX_URL_CHARS));
        let result = run(&tool, FetchUrlTool::NAME, json!({ "url": long })).await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("8192"), "{}", result.content);
    }

    /// The API path does not require this machine to resolve the URL: the page
    /// is fetched on DeepSeek's network, so a lookup that fails here decides
    /// nothing — and behind an egress proxy (no direct DNS) requiring it
    /// refused *every* public URL while `web_search` kept working.
    ///
    /// `invalid` is reserved by RFC 6761 and never resolves, so the input is
    /// deterministic.
    #[tokio::test]
    async fn the_api_path_does_not_need_a_local_dns_answer() {
        let (base, _seen) =
            serve_api_once(200, json!({ "title": "t", "content": "page" }).to_string());
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://nonexistent.invalid/page" }),
        )
        .await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
    }

    /// …but a name this machine *can* see pointing inside the network is still
    /// refused before it is sent: handing it to a third party leaks it, and no
    /// proxy excuses that.
    #[tokio::test]
    async fn the_api_path_still_refuses_a_host_that_resolves_to_a_private_address() {
        let (base, _seen) =
            serve_api_once(200, json!({ "title": "t", "content": "page" }).to_string());
        let tool = FetchUrlTool::new(zh(), &WebBackend::test_api(&base));
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "http://localhost:9/" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("localhost"), "{}", result.content);
    }

    /// The local path keeps the resolving guard, because there the request
    /// leaves this machine: a host it cannot resolve is refused by the guard —
    /// not by a connection that then fails — rather than handed to a client
    /// that would resolve it again at connect time. (`pinned_get` repeats the
    /// check per redirect hop, so this is the first of two.)
    #[tokio::test]
    async fn the_local_path_still_refuses_a_host_it_cannot_resolve() {
        let tool = FetchUrlTool {
            allow_private: false,
            ..FetchUrlTool::new(zh(), &WebBackend::local_only())
        };
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "https://nonexistent.invalid/page" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            result.content.contains("无法解析主机"),
            "the guard's own refusal, not a failed connection: {}",
            result.content
        );
    }

    /// The local path (non-DeepSeek endpoints) still works: HTML is reduced to
    /// text, the raw download is capped with a note, and the same `offset`
    /// paging applies to whatever text survived.
    #[tokio::test]
    async fn fetch_url_locally_reads_caps_and_pages() {
        let url = serve_once("x".repeat(64), "text/plain");
        let tool = FetchUrlTool {
            max_bytes: 16,
            ..local_fetch_tool()
        };
        let result = run(&tool, FetchUrlTool::NAME, json!({ "url": url.clone() })).await;
        assert_eq!(
            result.content,
            format!(
                "{url}\n[下载已按 16 bytes 上限截断，下文为截断前的内容]\n\n{}",
                "x".repeat(16)
            )
        );

        let url = serve_once("<p>纯文本 &amp; tags</p>".to_string(), "text/html");
        let result = run(
            &local_fetch_tool(),
            FetchUrlTool::NAME,
            json!({ "url": url.clone() }),
        )
        .await;
        assert_eq!(result.content, format!("{url}\n\n纯文本 & tags"));

        let url = serve_once("abcdef".to_string(), "text/plain");
        let result = run(
            &local_fetch_tool(),
            FetchUrlTool::NAME,
            json!({ "url": url.clone(), "offset": 4 }),
        )
        .await;
        assert_eq!(result.content, format!("{url}\n\nef\n\n[内容结束]"));
        assert!(
            local_fetch_tool()
                .description()
                .contains("from this machine")
        );
    }

    #[tokio::test]
    async fn fetch_url_blocks_private_by_default() {
        let tool = FetchUrlTool::new(zh(), &WebBackend::local_only());
        let result = run(
            &tool,
            FetchUrlTool::NAME,
            json!({ "url": "http://127.0.0.1:9/" }),
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("非公网"));
    }

    #[test]
    fn network_tools_are_gated_read_only_medium() {
        use crate::execution_policy::{ExecPolicy, RiskLevel, ToolKind};
        for name in [WebSearchTool::NAME, FetchUrlTool::NAME] {
            assert_eq!(ExecPolicy::classify_tool(name), ToolKind::Network);
            let plan = ExecPolicy::new().evaluate_tool(name, &json!({}));
            assert!(plan.requires_approval, "{name} must be approval-gated");
            assert!(plan.read_only);
            assert_eq!(plan.risk_level, RiskLevel::Medium);
        }
    }

    #[tokio::test]
    async fn missing_arguments_are_invalid() {
        // Missing `url` fails serde parsing in the ErasedTool blanket impl
        // (message wording is serde's, e.g. "missing field `url`").
        let error = FetchUrlTool::new(zh(), &WebBackend::local_only())
            .execute(&call(FetchUrlTool::NAME, json!({})), &ToolCx::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn api_error_messages_prefer_the_structured_message() {
        assert_eq!(
            api_error_message(r#"{"error":{"message":"bad  thing","type":"x","param":null}}"#),
            "bad thing"
        );
        assert_eq!(api_error_message("<html>502</html>"), "<html>502</html>");
        assert!(api_error_message(&"y".repeat(500)).ends_with('…'));
    }

    /// Real-network checks; run manually with
    /// `DEEPSEEK_API_KEY=… cargo test -p deep-code-agent web_tools -- --ignored`.
    #[tokio::test]
    #[ignore = "requires network and a DeepSeek API key"]
    async fn tools_api_real_network() {
        let backend = WebBackend::from_config(&AgentConfig::default());
        assert_eq!(backend.id(), "deepseek-tools-api", "set DEEPSEEK_API_KEY");
        let result = run(
            &WebSearchTool::new(zh(), &backend),
            WebSearchTool::NAME,
            json!({ "query": "rust programming language", "max_results": 3 }),
        )
        .await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert!(result.content.starts_with("1. "));
        let result = run(
            &FetchUrlTool::new(zh(), &backend),
            FetchUrlTool::NAME,
            json!({ "url": "https://example.com/" }),
        )
        .await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert!(result.content.contains("Example Domain"));
    }
}
