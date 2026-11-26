use std::io::{self, BufWriter, Write};
use std::pin::Pin;
use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use reqwest::{Body, Response, StatusCode, Url};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult, redact_api_key};
use crate::event::{AgentEvent, chunk_to_events};
use crate::model::{ChatRequest, StreamChunk};

pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>>;

const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_POST_REDIRECTS: usize = 10;

#[async_trait]
pub trait LlmClient: Send + Sync {
    fn provider_name(&self) -> &'static str;

    fn model(&self) -> &str;

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream>;
}

#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    config: AgentConfig,
}

enum ChatBody {
    Inline(Vec<u8>),
    Spooled(NamedTempFile, u64),
}

impl DeepSeekClient {
    pub fn new(mut config: AgentConfig) -> AgentResult<Self> {
        config.model = config.resolved_model()?.to_owned();
        config.require_api_key()?;

        // Deliberately no `ClientBuilder::timeout`: that clock runs until the
        // response body is fully read, so it would kill any SSE stream longer
        // than the timeout. `config.timeout` guards only the open phase in
        // `stream_chat`; stream liveness is enforced by the chunk/total
        // guards in runtime/streaming.rs.
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            config,
        })
    }

    async fn read_api_error(&self, response: reqwest::Response) -> AgentError {
        let status = response.status();
        // Bound the diagnostic read independently after headers arrive.
        // Preserve the status so 429/5xx still reach the retry/fallback policy.
        let timeout = self
            .config
            .timeout
            .unwrap_or(ERROR_BODY_TIMEOUT)
            .min(ERROR_BODY_TIMEOUT);
        let (message, truncated) =
            match tokio::time::timeout(timeout, read_error_body(response)).await {
                Ok(Ok(body)) => body,
                Ok(Err(error)) => (
                    format!("{status} (error body could not be read: {error})"),
                    false,
                ),
                Err(_) => (format!("{status} (error body timed out)"), false),
            };
        let mut message = redact_api_key(self.config.api_key.as_deref(), message, truncated);
        if truncated {
            message.push_str("\n[error response truncated]");
        }
        AgentError::Api { status, message }
    }

    async fn post_chat(
        &self,
        url: impl reqwest::IntoUrl,
        body: Body,
        length: u64,
    ) -> AgentResult<Response> {
        Ok(self
            .http
            .post(url)
            .bearer_auth(self.config.require_api_key()?)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_LENGTH, length)
            .body(body)
            .send()
            .await?)
    }

    async fn send_chat(&self, body: ChatBody) -> AgentResult<Response> {
        let endpoint = self.config.chat_completions_url();
        let (file, length) = match body {
            ChatBody::Inline(bytes) => {
                let length = bytes.len() as u64;
                return self.post_chat(endpoint, Body::from(bytes), length).await;
            }
            ChatBody::Spooled(file, length) => (file, length),
        };

        let mut url = self.http.post(endpoint).build()?.url().clone();
        url.set_fragment(None);
        let mut followed = 0;
        loop {
            // A fresh reader has an independent cursor even when the server
            // returns a redirect before receiving the entire first upload.
            let reader = file.reopen().map_err(image_request_error)?;
            let response = self
                .post_chat(
                    url.clone(),
                    Body::from(tokio::fs::File::from_std(reader)),
                    length,
                )
                .await?;
            let Some(next) = image_post_redirect(&response, &url) else {
                return Ok(response);
            };
            if followed == MAX_POST_REDIRECTS {
                return Err(AgentError::Api {
                    status: response.status(),
                    message: "too many image upload redirects".into(),
                });
            }
            followed += 1;
            url = next;
        }
    }
}

fn image_post_redirect(response: &Response, posted_to: &Url) -> Option<Url> {
    // Reqwest follows method-changing redirects itself. Replay only a POST
    // still at the URL we sent, and keep the image and authorization on origin.
    if response.url() != posted_to
        || !matches!(
            response.status(),
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
        )
    {
        return None;
    }
    let location = std::str::from_utf8(response.headers().get(LOCATION)?.as_bytes()).ok()?;
    let mut next = posted_to.join(location).ok()?;
    next.set_fragment(None);
    (next.origin() == posted_to.origin() && next.username().is_empty() && next.password().is_none())
        .then_some(next)
}

#[async_trait]
impl LlmClient for DeepSeekClient {
    fn provider_name(&self) -> &'static str {
        "deepseek"
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    async fn stream_chat(&self, mut request: ChatRequest) -> AgentResult<AgentEventStream> {
        if request.model.is_empty() {
            request.model = self.config.model.clone();
        }
        request.stream = true;

        let body = if request
            .messages
            .iter()
            .any(|message| message.content.has_images())
        {
            let cancel = CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let (file, length) =
                tokio::task::spawn_blocking(move || spool_image_request(&request, &cancel))
                    .await
                    .map_err(image_request_error)?
                    .map_err(image_request_error)?;
            ChatBody::Spooled(file, length)
        } else {
            ChatBody::Inline(serde_json::to_vec(&request)?)
        };

        let send = self.send_chat(body);

        // `send()` resolves once response headers arrive, before the body, so
        // this bounds connect + request + headers without constraining how
        // long the SSE body may stream.
        let response = match self.config.timeout {
            Some(timeout) => tokio::time::timeout(timeout, send).await.map_err(|_| {
                AgentError::RequestTimeout {
                    seconds: timeout.as_secs(),
                }
            })??,
            None => send.await?,
        };

        if !response.status().is_success() {
            return Err(self.read_api_error(response).await);
        }

        let byte_stream = response.bytes_stream();
        // The same budget `runtime::streaming` enforces on decoded content —
        // see `SseDecoder::with_limit` for why the raw buffer needs it too.
        let raw_limit = self.config.stream_max_bytes;
        let stream = try_stream! {
            let mut decoder = SseDecoder::with_limit(raw_limit);
            futures_util::pin_mut!(byte_stream);

            while let Some(bytes) = byte_stream.next().await {
                let mut events = Vec::new();
                let result = decoder.push(&bytes?, &mut events);
                for event in events {
                    yield event;
                }
                result?;
            }

            for event in decoder.finish()? {
                yield event;
            }
        };

        let api_key = self.config.api_key.clone();
        Ok(Box::pin(stream.map(move |event| match event {
            Ok(AgentEvent::Error { message }) => Ok(AgentEvent::Error {
                message: redact_api_key(api_key.as_deref(), message, false),
            }),
            other => other,
        })))
    }
}

fn image_request_error(error: impl std::fmt::Display) -> AgentError {
    AgentError::Attachment(format!(
        "图片请求准备失败 / could not prepare the image request: {error}"
    ))
}

/// Store the encoded request in a private temporary file so every image is
/// verified before upload. Reqwest reads from that file with backpressure.
fn spool_image_request(
    request: &ChatRequest,
    cancel: &CancellationToken,
) -> io::Result<(NamedTempFile, u64)> {
    check_spool_cancelled(cancel)?;
    let mut writer = SpoolWriter {
        file: BufWriter::new(NamedTempFile::new()?),
        cancel,
    };
    serde_json::to_writer(&mut writer, request)?;
    writer.flush()?;
    let file = writer
        .file
        .into_inner()
        .map_err(|error| error.into_error())?;
    let length = file.as_file().metadata()?.len();
    Ok((file, length))
}

struct SpoolWriter<'a, W> {
    file: W,
    cancel: &'a CancellationToken,
}

impl<W: Write> Write for SpoolWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_spool_cancelled(self.cancel)?;
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        check_spool_cancelled(self.cancel)?;
        self.file.flush()
    }
}

fn check_spool_cancelled(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        // Write::write_all retries ErrorKind::Interrupted.
        return Err(io::Error::other("image request was cancelled"));
    }
    Ok(())
}

async fn read_error_body(response: reqwest::Response) -> Result<(String, bool), reqwest::Error> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let take = chunk.len().min(MAX_ERROR_BODY_BYTES + 1 - bytes.len());
        bytes.extend_from_slice(&chunk[..take]);
        if bytes.len() > MAX_ERROR_BODY_BYTES {
            bytes.truncate(MAX_ERROR_BODY_BYTES);
            return Ok((String::from_utf8_lossy(&bytes).into_owned(), true));
        }
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), false))
}

/// Incremental SSE decoder.
///
/// Buffers raw bytes and only decodes complete lines: chunk boundaries are set
/// by TCP/TLS framing and can split a multi-byte UTF-8 character (any CJK char
/// is 3 bytes), so per-chunk `from_utf8` would abort the stream. `\n` is a
/// single byte that never occurs inside a UTF-8 sequence, which makes
/// line-level decoding safe.
///
/// Follows the SSE spec: `data:` field lines accumulate (joined with `\n`)
/// until a blank line dispatches the event; comment lines (`:`) and other
/// fields (`event:`, `id:`, `retry:`) are ignored.
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
    /// Ceiling on what may sit undispatched here — see [`Self::with_limit`].
    limit_bytes: u64,
}

impl SseDecoder {
    /// A decoder bounded by `limit_bytes` (`stream.max_bytes`).
    ///
    /// The guard in `runtime::streaming` counts DECODED event content, so it
    /// bounds nothing that never becomes an event. Two things here do:
    /// `buffer` holds bytes until a `\n` arrives, and `data` accumulates
    /// `data:` field lines until a blank line dispatches them. A peer that
    /// sends neither — a stuck proxy, a misbehaving endpoint, a `base_url` the
    /// user pointed somewhere unexpected — grows both without limit, and the
    /// liveness guards do not fire while bytes keep arriving: the chunk
    /// timeout is reset by every chunk, so only the total deadline (default
    /// 900s) ends it. That is fifteen minutes of unbounded allocation behind a
    /// setting whose documented job is to stop exactly this.
    ///
    /// The limit is charged against the two buffers together, because they are
    /// two halves of one undispatched event.
    pub(crate) fn with_limit(limit_bytes: u64) -> Self {
        Self {
            buffer: Vec::new(),
            data: String::new(),
            limit_bytes,
        }
    }

    /// A decoder with a limit no test payload reaches, so the tests below keep
    /// asserting about framing rather than about the ceiling. The ceiling has
    /// its own test (`an_endless_line_is_refused_instead_of_buffered`).
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_limit(u64::MAX)
    }

    /// Bytes held that have not yet become an event.
    fn buffered(&self) -> u64 {
        self.buffer.len() as u64 + self.data.len() as u64
    }

    fn check_limit(&self, additional: usize) -> AgentResult<()> {
        if self.buffered().saturating_add(additional as u64) > self.limit_bytes {
            return Err(AgentError::StreamOverflow {
                limit_bytes: self.limit_bytes,
            });
        }
        Ok(())
    }

    /// Feed one network chunk, retaining completed events before a later error.
    fn push(&mut self, bytes: &[u8], events: &mut Vec<AgentEvent>) -> AgentResult<()> {
        for fragment in bytes.split_inclusive(|&byte| byte == b'\n') {
            self.check_limit(fragment.len())?;
            let Some(line) = fragment.strip_suffix(b"\n") else {
                self.buffer.extend_from_slice(fragment);
                break;
            };
            if self.buffer.is_empty() {
                self.handle_line(line, events)?;
            } else {
                let mut line_bytes = std::mem::take(&mut self.buffer);
                line_bytes.extend_from_slice(line);
                self.handle_line(&line_bytes, events)?;
            }
        }
        Ok(())
    }

    /// Flush a trailing event that was never terminated by a blank line.
    pub(crate) fn finish(&mut self) -> AgentResult<Vec<AgentEvent>> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let tail = std::mem::take(&mut self.buffer);
            self.handle_line(&tail, &mut events)?;
        }
        self.dispatch(&mut events)?;
        Ok(events)
    }

    fn handle_line(&mut self, line: &[u8], events: &mut Vec<AgentEvent>) -> AgentResult<()> {
        let line = std::str::from_utf8(line)
            .map_err(|error| AgentError::Parse(error.to_string()))?
            .trim_end_matches('\r');
        if line.is_empty() {
            return self.dispatch(events);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<AgentEvent>) -> AgentResult<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data);
        events.extend(parse_sse_data(&data)?);
        Ok(())
    }
}

pub(crate) fn parse_sse_data(data: &str) -> AgentResult<Vec<AgentEvent>> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(Vec::new());
    }

    #[derive(serde::Deserialize)]
    struct ErrorFrame {
        error: Option<serde::de::IgnoredAny>,
    }
    if serde_json::from_str::<ErrorFrame>(data).is_ok_and(|frame| frame.error.is_some()) {
        return Ok(vec![AgentEvent::Error {
            message: data.to_owned(),
        }]);
    }
    Ok(chunk_to_events(serde_json::from_str::<StreamChunk>(data)?))
}

#[cfg(test)]
#[path = "client/http_tests.rs"]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_during_serialization_stops_writes_and_drops_the_temporary_file() {
        struct CancelOnWrite<'a>(NamedTempFile, &'a CancellationToken, usize);
        impl Write for CancelOnWrite<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.2 += 1;
                self.1.cancel();
                self.0.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        let cancel = CancellationToken::new();
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let mut writer = SpoolWriter {
            file: CancelOnWrite(file, &cancel, 0),
            cancel: &cancel,
        };
        let error = serde_json::to_writer(&mut writer, &ChatRequest::streaming("test", vec![]))
            .unwrap_err();
        assert!(error.to_string().contains("image request was cancelled"));
        assert_eq!(writer.file.2, 1);
        drop(writer);
        assert!(!path.exists());
    }

    #[test]
    fn sse_error_preserves_the_provider_payload() {
        for body in [
            r#"{"error":{"message":"input rejected","code":"invalid_request"}}"#,
            r#"{"choices":[{"finish_reason":"error"}],"error":{"message":"input rejected"}}"#,
        ] {
            assert_eq!(
                parse_sse_data(body).unwrap(),
                [AgentEvent::Error {
                    message: body.into()
                }]
            );
        }
    }

    const CHUNK: &str = r#"{"id":"1","model":"deepseek-v4-pro","choices":[{"index":0,"message":null,"delta":{"role":null,"content":"hi"},"finish_reason":null}],"usage":null}"#;

    fn text_delta(text: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            text: text.to_string(),
        }
    }

    fn decode_chunk(decoder: &mut SseDecoder, bytes: &[u8]) -> AgentResult<Vec<AgentEvent>> {
        let mut events = Vec::new();
        decoder.push(bytes, &mut events)?;
        Ok(events)
    }

    #[test]
    fn parse_sse_data_treats_done_marker_as_transport_terminator() {
        assert_eq!(parse_sse_data("[DONE]").unwrap(), Vec::new());
    }

    #[test]
    fn parse_sse_data_maps_json_chunk() {
        assert_eq!(parse_sse_data(CHUNK).unwrap(), vec![text_delta("hi")]);
    }

    #[test]
    fn decoder_dispatches_event_on_blank_line() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {CHUNK}\n\n");
        assert_eq!(
            decode_chunk(&mut decoder, frame.as_bytes()).unwrap(),
            vec![text_delta("hi")]
        );
    }

    #[test]
    fn decoder_keeps_completed_events_before_a_bad_frame_in_the_same_chunk() {
        let first = format!("data: {CHUNK}\n\n");
        let mut decoder = SseDecoder::with_limit(first.len() as u64);
        let mut events = Vec::new();
        let malformed = format!("{first}data: {{\n\n");
        let error = decoder.push(malformed.as_bytes(), &mut events).unwrap_err();
        assert!(matches!(error, AgentError::Serde(_)));
        assert_eq!(events, vec![text_delta("hi")]);

        let mut decoder = SseDecoder::with_limit(first.len() as u64);
        events.clear();
        let oversized = format!("{first}{}", "x".repeat(first.len() + 1));
        let error = decoder.push(oversized.as_bytes(), &mut events).unwrap_err();
        assert!(matches!(error, AgentError::StreamOverflow { .. }));
        assert_eq!(events, vec![text_delta("hi")]);
    }

    #[test]
    fn decoder_survives_utf8_char_split_across_chunks() {
        let payload = CHUNK.replace(r#""content":"hi""#, r#""content":"你好""#);
        let frame = format!("data: {payload}\n\n");
        let bytes = frame.as_bytes();
        // Cut one byte into 你's three-byte UTF-8 sequence.
        let split = frame.find('你').unwrap() + 1;

        let mut decoder = SseDecoder::new();
        assert_eq!(
            decode_chunk(&mut decoder, &bytes[..split]).unwrap(),
            Vec::new()
        );
        assert_eq!(
            decode_chunk(&mut decoder, &bytes[split..]).unwrap(),
            vec![text_delta("你好")]
        );
    }

    #[test]
    fn decoder_handles_crlf_lines() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {CHUNK}\r\n\r\n");
        assert_eq!(
            decode_chunk(&mut decoder, frame.as_bytes()).unwrap(),
            vec![text_delta("hi")]
        );
    }

    #[test]
    fn decoder_joins_multi_line_data_fields() {
        // SSE spec: consecutive `data:` lines are joined with `\n` before
        // dispatch; `\n` between JSON tokens is legal whitespace.
        let (head, tail) = CHUNK.split_at(CHUNK.find(r#""choices""#).unwrap());
        let frame = format!("data: {head}\ndata: {tail}\n\n");
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decode_chunk(&mut decoder, frame.as_bytes()).unwrap(),
            vec![text_delta("hi")]
        );
    }

    #[test]
    fn decoder_ignores_comment_and_done_frames() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decode_chunk(&mut decoder, b": keep-alive\n\ndata: [DONE]\n\n").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn decoder_finish_flushes_unterminated_trailing_event() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {CHUNK}");
        assert_eq!(
            decode_chunk(&mut decoder, frame.as_bytes()).unwrap(),
            Vec::new()
        );
        assert_eq!(decoder.finish().unwrap(), vec![text_delta("hi")]);
    }

    /// Real-network smoke for the full request/auth/SSE path; run manually:
    /// `DEEPSEEK_API_KEY=... cargo test -p deep-code-agent client -- --ignored`
    #[tokio::test]
    #[ignore = "requires DEEPSEEK_API_KEY and network"]
    async fn real_deepseek_streams_text_and_done() {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .expect("set DEEPSEEK_API_KEY to run this smoke test");
        let config = AgentConfig {
            api_key: Some(api_key),
            model: crate::model_registry::DEEPSEEK_FLASH.to_string(),
            ..AgentConfig::builtin()
        };
        let client = DeepSeekClient::new(config.clone()).expect("client builds");
        let mut request = ChatRequest::streaming(
            config.model.clone(),
            vec![crate::message::Message::user("Reply with exactly: pong")],
        )
        .with_reasoning_effort("none");
        request.max_tokens = Some(32);

        // This calls the client directly, outside the runtime's stream guards.
        // Bound the body as well as the connection so a stalled service cannot
        // leave a canary open indefinitely.
        tokio::time::timeout(Duration::from_secs(60), async {
            let mut events = client.stream_chat(request).await.expect("stream opens");
            let mut text = String::new();
            let mut saw_done = false;
            while let Some(event) = events.next().await {
                match event.expect("stream event decodes") {
                    AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                    AgentEvent::Done { .. } => saw_done = true,
                    _ => {}
                }
            }
            assert!(saw_done, "stream must terminate with a Done event");
            assert!(
                !text.trim().is_empty(),
                "model must stream non-empty text, got none"
            );
        })
        .await
        .expect("real SSE smoke exceeded 60 seconds");
    }

    /// `stream.max_bytes` is documented as the stream's size guard, and the
    /// guard in `runtime::streaming` only sees DECODED content. A peer that
    /// never sends a newline produces no events at all, so nothing there could
    /// ever fire; the bytes simply accumulated here.
    #[test]
    fn an_endless_line_is_refused_instead_of_buffered() {
        let mut decoder = SseDecoder::with_limit(1_024);
        let chunk = vec![b'x'; 512];
        assert!(decode_chunk(&mut decoder, &chunk).unwrap().is_empty());
        assert!(decode_chunk(&mut decoder, &chunk).unwrap().is_empty());
        let error = decode_chunk(&mut decoder, &chunk).unwrap_err();
        assert!(
            matches!(error, AgentError::StreamOverflow { limit_bytes } if limit_bytes == 1_024),
            "got {error:?}"
        );
    }

    /// The other half of the same budget: `data:` lines with no blank line
    /// between them accumulate in `data`, not in `buffer`.
    #[test]
    fn endless_undispatched_data_lines_are_refused() {
        let mut decoder = SseDecoder::with_limit(1_024);
        let line = format!("data: {}\n", "y".repeat(256));
        let mut last = Ok(Vec::new());
        for _ in 0..20 {
            last = decode_chunk(&mut decoder, line.as_bytes());
            if last.is_err() {
                break;
            }
        }
        assert!(
            matches!(last, Err(AgentError::StreamOverflow { .. })),
            "expected the accumulating data field to be bounded too"
        );
    }

    /// The bound must not cost an ordinary stream: a complete event is
    /// dispatched and its bytes stop counting against the ceiling.
    #[test]
    fn dispatched_events_release_their_bytes() {
        let frame = format!("data: {CHUNK}\n\n");
        // Room for one frame and not much more: if a dispatched event kept
        // charging against the ceiling, the second push would already fail.
        let mut decoder = SseDecoder::with_limit(frame.len() as u64 + 8);
        for _ in 0..50 {
            let events = decode_chunk(&mut decoder, frame.as_bytes())
                .expect("a dispatched frame must not accumulate");
            assert_eq!(events, vec![text_delta("hi")]);
        }
    }
}
