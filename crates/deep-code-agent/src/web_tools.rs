//! Network tools: `web_search` (DuckDuckGo, keyless) and `fetch_url`.
//!
//! Both wrap their blocking `reqwest` bodies in [`crate::tool::run_blocking`],
//! so requests land on the spawn_blocking pool, and are gated by
//! the execution policy (`ToolKind::Network` → approval required); session
//! approval / `auto_allow` are the intended low-friction paths.
//!
//! SSRF hardening: loopback, RFC1918, link-local, CGNAT, and their IPv6
//! equivalents are rejected, and every redirect hop is re-checked (5 hops
//! max). Validation is connect-time, not just check-time: each hop resolves
//! the host once, validates every address, then pins the connection to a
//! validated address via the client's DNS override, so a rebinding DNS server
//! cannot swap in a private address between check and connect.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::i18n::{Lang, SharedLang, TextId, tr, tr_with};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, run_blocking};

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;
const DEFAULT_MAX_FETCH_BYTES: usize = 512 * 1024;
const DEFAULT_SEARCH_RESULTS: usize = 5;
const MAX_SEARCH_RESULTS: usize = 10;
const USER_AGENT: &str = concat!("deep-code/", env!("CARGO_PKG_VERSION"));

/// Registry with both network tools, for [`crate::runtime_launch`]. The
/// shared language handle keeps tool error text in step with `/lang`.
#[must_use]
pub fn web_tool_registry(ui_lang: &SharedLang) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(WebSearchTool::new(ui_lang.clone()));
    registry.register(FetchUrlTool::new(ui_lang.clone()));
    registry
}

#[derive(Debug, Clone)]
pub struct FetchUrlTool {
    /// Test hook: lift the public-address requirement so unit tests can hit
    /// a local server. Never set in production registries.
    allow_private: bool,
    max_bytes: usize,
    lang: SharedLang,
}

impl FetchUrlTool {
    pub const NAME: &'static str = "fetch_url";

    #[must_use]
    pub fn new(lang: SharedLang) -> Self {
        Self {
            allow_private: false,
            max_bytes: DEFAULT_MAX_FETCH_BYTES,
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
        let response = match pinned_get(parsed, self.allow_private, lang) {
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
        let truncated = body.len() > self.max_bytes;
        body.truncate(self.max_bytes);
        let raw = String::from_utf8_lossy(&body);

        let mut text = if content_type.contains("text/html") {
            html_to_text(&raw)
        } else {
            raw.to_string()
        };
        if truncated {
            text.push_str("\n\n");
            text.push_str(tr(lang, TextId::WebContentTruncated));
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchUrlParams {
    /// Absolute http or https URL to fetch.
    url: String,
}

#[async_trait]
impl Tool for FetchUrlTool {
    type Params = FetchUrlParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Fetch a public http(s) URL and return its text content (HTML is reduced to readable text). Private/internal addresses are rejected."
    }

    async fn run(&self, params: FetchUrlParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.fetch_sync(params)).await
    }
}

#[derive(Debug, Clone)]
pub struct WebSearchTool {
    lang: SharedLang,
}

impl WebSearchTool {
    pub const NAME: &'static str = "web_search";

    #[must_use]
    pub fn new(lang: SharedLang) -> Self {
        Self { lang }
    }

    fn search_sync(&self, params: WebSearchParams) -> Result<ToolOutput, ToolError> {
        let lang = self.lang.get();
        let query = params.query.as_str();
        if query.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                name: Self::NAME.to_string(),
                message: "missing string field 'query'".to_string(),
            });
        }
        let max_results = params.max_results.map_or(DEFAULT_SEARCH_RESULTS, |value| {
            value.clamp(1, MAX_SEARCH_RESULTS)
        });

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );
        let parsed = match Url::parse(&url) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(ToolOutput::soft_error(tr_with(
                    lang,
                    TextId::WebSearchUrlError,
                    &[("error", &error.to_string())],
                )));
            }
        };
        let response = match pinned_get(parsed, false, lang) {
            Ok(response) => response,
            Err(error) => {
                return Ok(ToolOutput::soft_error(tr_with(
                    lang,
                    TextId::WebSearchRequestError,
                    &[("error", &error)],
                )));
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::WebSearchHttpStatus,
                &[("status", &status.to_string())],
            )));
        }
        let html = match response.text() {
            Ok(html) => html,
            Err(error) => {
                return Ok(ToolOutput::soft_error(tr_with(
                    lang,
                    TextId::WebSearchReadError,
                    &[("error", &error.to_string())],
                )));
            }
        };

        let results = parse_search_results(&html, max_results);
        if results.is_empty() {
            return Ok(ToolOutput::soft_error(
                tr(lang, TextId::WebSearchNoResults).to_string(),
            ));
        }
        let mut out = String::new();
        for (index, result) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n",
                index + 1,
                result.title,
                result.url,
                result.snippet
            ));
        }
        Ok(ToolOutput::text(out))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchParams {
    /// Search query.
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
        "Search the web (DuckDuckGo) and return the top results with title, URL and snippet."
    }

    async fn run(&self, params: WebSearchParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.search_sync(params)).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// GET with manual redirect handling. Each hop's host is resolved and
/// validated exactly once, and the connection is pinned to the validated
/// address, closing the check-time/connect-time DNS rebinding gap.
fn pinned_get(
    start: Url,
    allow_private: bool,
    lang: Lang,
) -> Result<reqwest::blocking::Response, String> {
    let mut url = start;
    for _ in 0..=MAX_REDIRECTS {
        let pin = check_url(&url, allow_private, lang)?;
        let mut builder = Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(Policy::none());
        if let Some(addr) = pin
            && let Some(host) = url.host_str()
        {
            builder = builder.resolve(host, addr);
        }
        let client = builder.build().map_err(|error| {
            tr_with(
                lang,
                TextId::WebClientInitError,
                &[("error", &error.to_string())],
            )
        })?;
        let response = client.get(url.clone()).send().map_err(|error| {
            tr_with(
                lang,
                TextId::WebRequestError,
                &[("error", &error.to_string())],
            )
        })?;
        if response.status().is_redirection()
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
        {
            url = url.join(location).map_err(|error| {
                tr_with(
                    lang,
                    TextId::WebRedirectInvalid,
                    &[("error", &error.to_string())],
                )
            })?;
            continue;
        }
        return Ok(response);
    }
    Err(tr(lang, TextId::WebRedirectLimit).to_string())
}

/// Reject non-http(s) schemes and any host that resolves to a non-public
/// address (SSRF guard). For domain hosts, returns the validated address the
/// caller must pin its connection to; IP-literal hosts involve no DNS and
/// need no pin. `allow_private` is a test-only escape hatch.
fn check_url(url: &Url, allow_private: bool, lang: Lang) -> Result<Option<SocketAddr>, String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(tr_with(
                lang,
                TextId::WebSchemeNotAllowed,
                &[("scheme", other)],
            ));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| tr(lang, TextId::WebUrlNoHost).to_string())?;
    if allow_private {
        return Ok(None);
    }
    // IP-literal host: no resolution happens, so validate the literal directly.
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        if !ip_is_public(ip) {
            return Err(tr_with(
                lang,
                TextId::WebPrivateHostBlocked,
                &[("host", host)],
            ));
        }
        return Ok(None);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            tr_with(
                lang,
                TextId::WebHostResolveError,
                &[("host", host), ("error", &error.to_string())],
            )
        })?
        .collect();
    if addrs.is_empty() {
        return Err(tr_with(lang, TextId::WebHostNoAddrs, &[("host", host)]));
    }
    for addr in &addrs {
        if !ip_is_public(addr.ip()) {
            return Err(tr_with(
                lang,
                TextId::WebPrivateResolvedBlocked,
                &[("host", host), ("addr", &addr.ip().to_string())],
            ));
        }
    }
    Ok(Some(addrs[0]))
}

fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => ipv6_is_public(v6),
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || cgnat)
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_public(v4);
    }
    let segments = ip.segments();
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local)
}

/// Crude but dependency-free HTML → text: drop script/style, strip tags,
/// decode common entities, collapse blank lines.
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
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
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

/// Parse DuckDuckGo's HTML results page: pairs of `result__a` (title+href,
/// with the target wrapped in a `uddg` parameter) and `result__snippet`.
fn parse_search_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET_RE: OnceLock<Regex> = OnceLock::new();
    let link_re = LINK_RE.get_or_init(|| {
        Regex::new(r#"(?s)class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
    });
    let snippet_re = SNIPPET_RE
        .get_or_init(|| Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap());

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|captures| html_to_text(&captures[1]))
        .collect();

    link_re
        .captures_iter(html)
        .take(max_results)
        .enumerate()
        .map(|(index, captures)| SearchResult {
            title: html_to_text(&captures[2]),
            url: unwrap_ddg_link(&decode_entities(&captures[1])),
            snippet: snippets.get(index).cloned().unwrap_or_default(),
        })
        .collect()
}

/// DDG wraps targets as `//duckduckgo.com/l/?uddg=<urlencoded>&rut=...`.
fn unwrap_ddg_link(href: &str) -> String {
    if let Some(start) = href.find("uddg=") {
        let encoded = &href[start + 5..];
        let encoded = encoded.split('&').next().unwrap_or(encoded);
        return percent_encoding::percent_decode_str(encoded)
            .decode_utf8_lossy()
            .into_owned();
    }
    href.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    use serde_json::{Value, json};

    use crate::tool::{ErasedTool, ToolCall, ToolCx};

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new("call_1", name, arguments)
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

    #[test]
    fn ssrf_guard_rejects_private_hosts() {
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert!(
                check_url(&parsed, false, Lang::Zh).is_err(),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn ssrf_guard_accepts_public_literals_and_rejects_schemes() {
        let public = Url::parse("https://1.1.1.1/").unwrap();
        assert!(check_url(&public, false, Lang::Zh).is_ok());
        let file = Url::parse("file:///etc/passwd").unwrap();
        assert!(check_url(&file, false, Lang::Zh).is_err());
        let ftp = Url::parse("ftp://example.com/").unwrap();
        assert!(check_url(&ftp, false, Lang::Zh).is_err());
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

    #[test]
    fn parses_ddg_results_with_uddg_unwrap() {
        let html = r##"
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=abc">Example <b>Docs</b></a>
            <a class="result__snippet" href="#">The official <b>docs</b> site.</a>
            <a rel="nofollow" class="result__a" href="https://direct.example.com/">Direct</a>
            <a class="result__snippet" href="#">Second snippet.</a>
        "##;
        let results = parse_search_results(html, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Example Docs");
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].snippet, "The official docs site.");
        assert_eq!(results[1].url, "https://direct.example.com/");

        let capped = parse_search_results(html, 1);
        assert_eq!(capped.len(), 1);
    }

    #[tokio::test]
    async fn fetch_url_reads_local_server_and_truncates() {
        let url = serve_once("x".repeat(64), "text/plain");
        let tool = FetchUrlTool {
            allow_private: true,
            max_bytes: 16,
            lang: SharedLang::new(Lang::Zh),
        };
        let result = ErasedTool::execute(
            &tool,
            &call(FetchUrlTool::NAME, json!({ "url": url })),
            &ToolCx::default(),
        )
        .await
        .unwrap();
        assert!(result.content.starts_with("xxxx"));
        assert!(result.content.contains("截断"));

        let url = serve_once("<p>纯文本 &amp; tags</p>".to_string(), "text/html");
        let tool = FetchUrlTool {
            allow_private: true,
            max_bytes: DEFAULT_MAX_FETCH_BYTES,
            lang: SharedLang::new(Lang::Zh),
        };
        let result = ErasedTool::execute(
            &tool,
            &call(FetchUrlTool::NAME, json!({ "url": url })),
            &ToolCx::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.content, "纯文本 & tags");
    }

    #[tokio::test]
    async fn fetch_url_blocks_private_by_default() {
        let tool = FetchUrlTool::new(SharedLang::new(Lang::Zh));
        let result = ErasedTool::execute(
            &tool,
            &call(FetchUrlTool::NAME, json!({ "url": "http://127.0.0.1:9/" })),
            &ToolCx::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.status, crate::tool::ToolResultStatus::Error);
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
        // Missing `url` now fails serde parsing in the ErasedTool blanket impl
        // (message wording is serde's, e.g. "missing field `url`").
        let error = ErasedTool::execute(
            &FetchUrlTool::new(SharedLang::new(Lang::Zh)),
            &call(FetchUrlTool::NAME, json!({})),
            &ToolCx::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));

        // Blank query is value-level validation inside the tool body.
        let error = ErasedTool::execute(
            &WebSearchTool::new(SharedLang::new(Lang::Zh)),
            &call(WebSearchTool::NAME, json!({ "query": " " })),
            &ToolCx::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));
    }

    /// Real-network checks; run manually with
    /// `cargo test -p deep-code-agent web_tools -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetch_url_real_network() {
        let result = ErasedTool::execute(
            &FetchUrlTool::new(SharedLang::new(Lang::Zh)),
            &call(FetchUrlTool::NAME, json!({ "url": "https://example.com/" })),
            &ToolCx::default(),
        )
        .await
        .unwrap();
        assert!(result.content.contains("Example Domain"));
    }

    #[tokio::test]
    #[ignore]
    async fn web_search_real_network() {
        let result = ErasedTool::execute(
            &WebSearchTool::new(SharedLang::new(Lang::Zh)),
            &call(
                WebSearchTool::NAME,
                json!({ "query": "rust programming language", "max_results": 3 }),
            ),
            &ToolCx::default(),
        )
        .await
        .unwrap();
        assert!(result.content.contains("1. "));
    }
}
