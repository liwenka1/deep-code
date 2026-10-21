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
//! The SSRF guard runs on every URL the model hands in, on BOTH fetch paths.
//! When DeepSeek's service does the fetching it is their network that reaches
//! the target, so the guard protects nothing on this host — but a loopback
//! literal or an internal hostname is not a research target, and sending it to
//! a third party would leak the name of something behind the user's firewall.
//! Refusing first is the honest answer either way.
//!
//! Both wrap their blocking `reqwest` bodies in [`crate::tool::run_blocking`],
//! so requests land on the spawn_blocking pool.

use std::io::Read;
use std::sync::OnceLock;
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
ends with a truncation note, call again with the `offset` it names to continue. Private/internal \
addresses and URLs with embedded credentials are refused.";
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
    let response = client
        .post(format!("{}/{path}", api.base))
        .bearer_auth(&api.api_key)
        .json(body)
        .send()
        .map_err(|error| {
            tr_with(
                lang,
                TextId::WebApiRequestError,
                &[("error", &error.to_string())],
            )
        })?;
    let status = response.status();
    let text = response.text().map_err(|error| {
        tr_with(
            lang,
            TextId::WebReadBodyError,
            &[("error", &error.to_string())],
        )
    })?;
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(tr(lang, TextId::WebApiRateLimited).to_string());
    }
    if !status.is_success() {
        return Err(tr_with(
            lang,
            TextId::WebApiHttpStatus,
            &[
                ("status", &status.to_string()),
                ("message", &api_error_message(&text)),
            ],
        ));
    }
    serde_json::from_str(&text).map_err(|error| {
        tr_with(
            lang,
            TextId::WebApiBadResponse,
            &[("error", &error.to_string())],
        )
    })
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
        // Before any request leaves this process — see the module docs for why
        // the API path checks too. `pinned_get` repeats it per redirect hop.
        if let Err(reason) = ssrf::check_url(&parsed, self.allow_private, lang) {
            return Ok(ToolOutput::soft_error(reason));
        }
        let offset = params.offset.unwrap_or(0);

        let (title, content, source_note) = match &self.api {
            Some(api) => {
                if params.url.chars().count() > MAX_URL_CHARS {
                    return Ok(ToolOutput::soft_error(tr_with(
                        lang,
                        TextId::WebUrlTooLong,
                        &[("max", &MAX_URL_CHARS.to_string())],
                    )));
                }
                let response = match api_post(
                    api,
                    "fetch",
                    &json!({ "url": parsed.as_str() }),
                    FETCH_TIMEOUT,
                    lang,
                ) {
                    Ok(response) => response,
                    Err(reason) => return Ok(ToolOutput::soft_error(reason)),
                };
                let Some(content) = response.get("content").and_then(Value::as_str) else {
                    return Ok(ToolOutput::soft_error(tr_with(
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
                    Err(reason) => return Ok(ToolOutput::soft_error(reason)),
                };
                let status = response.status();
                if !status.is_success() {
                    return Ok(ToolOutput::soft_error(tr_with(
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
                    return Ok(ToolOutput::soft_error(tr_with(
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
                    capped.then(|| tr(lang, TextId::WebContentTruncated)),
                )
            }
        };

        match render_fetched(&parsed, &title, &content, source_note, offset, lang) {
            Ok(text) => Ok(ToolOutput::text(text)),
            Err(reason) => Ok(ToolOutput::soft_error(reason)),
        }
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
            hit.url
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
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buffer[..split]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                let length = header("content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let body_start = split + 4;
                if buffer.len() < body_start + length {
                    continue;
                }
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                let body = serde_json::from_slice(&buffer[body_start..body_start + length])
                    .unwrap_or(Value::Null);
                let _ = tx.send(Captured {
                    path,
                    authorization: header("authorization"),
                    body,
                });
                break;
            }
            let response = format!(
                "HTTP/1.1 {status} Whatever\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://127.0.0.1:{port}"), rx)
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
                "{url}\n[下载已按 512 KiB 上限截断，下文为截断前的内容]\n\n{}",
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
