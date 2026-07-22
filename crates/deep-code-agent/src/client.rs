use std::future::Future;
use std::pin::Pin;

use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult};
use crate::event::{AgentEvent, chunk_to_events};
use crate::model::{ChatRequest, StreamChunk};

pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>>;

pub trait LlmClient: Send + Sync {
    fn provider_name(&self) -> &'static str;

    fn model(&self) -> &str;

    fn stream_chat(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = AgentResult<AgentEventStream>> + Send;
}

#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    config: AgentConfig,
}

impl DeepSeekClient {
    pub fn new(config: AgentConfig) -> AgentResult<Self> {
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

    #[must_use]
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }
}

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

        let send = self
            .http
            .post(self.config.chat_completions_url())
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.require_api_key()?),
            )
            .header(CONTENT_TYPE, "application/json")
            .json(&request)
            .send();

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
            let status = response.status();
            let message = response.text().await.unwrap_or_else(|_| status.to_string());
            return Err(AgentError::Api { status, message });
        }

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut decoder = SseDecoder::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(bytes) = byte_stream.next().await {
                let bytes = bytes?;
                for event in decoder.push(&bytes)? {
                    yield event;
                }
            }

            for event in decoder.finish()? {
                yield event;
            }
        };

        Ok(Box::pin(stream))
    }
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
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            data: String::new(),
        }
    }

    /// Feed one network chunk; returns the events it completed.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> AgentResult<Vec<AgentEvent>> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = std::str::from_utf8(&line_bytes[..line_bytes.len() - 1])
                .map_err(|error| AgentError::Parse(error.to_string()))?
                .trim_end_matches('\r');
            self.handle_line(line, &mut events)?;
        }
        Ok(events)
    }

    /// Flush a trailing event that was never terminated by a blank line.
    pub(crate) fn finish(&mut self) -> AgentResult<Vec<AgentEvent>> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let tail = std::mem::take(&mut self.buffer);
            let line = std::str::from_utf8(&tail)
                .map_err(|error| AgentError::Parse(error.to_string()))?
                .trim_end_matches('\r');
            self.handle_line(line, &mut events)?;
        }
        self.dispatch(&mut events)?;
        Ok(events)
    }

    fn handle_line(&mut self, line: &str, events: &mut Vec<AgentEvent>) -> AgentResult<()> {
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

    let chunk: StreamChunk = serde_json::from_str(data)?;
    Ok(chunk_to_events(chunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: &str = r#"{"id":"1","model":"deepseek-v4-pro","choices":[{"index":0,"message":null,"delta":{"role":null,"content":"hi"},"finish_reason":null}],"usage":null}"#;

    fn text_delta(text: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            text: text.to_string(),
        }
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
            decoder.push(frame.as_bytes()).unwrap(),
            vec![text_delta("hi")]
        );
    }

    #[test]
    fn decoder_survives_utf8_char_split_across_chunks() {
        let payload = CHUNK.replace(r#""content":"hi""#, r#""content":"你好""#);
        let frame = format!("data: {payload}\n\n");
        let bytes = frame.as_bytes();
        // Cut one byte into 你's three-byte UTF-8 sequence.
        let split = frame.find('你').unwrap() + 1;

        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push(&bytes[..split]).unwrap(), Vec::new());
        assert_eq!(
            decoder.push(&bytes[split..]).unwrap(),
            vec![text_delta("你好")]
        );
    }

    #[test]
    fn decoder_handles_crlf_lines() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {CHUNK}\r\n\r\n");
        assert_eq!(
            decoder.push(frame.as_bytes()).unwrap(),
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
            decoder.push(frame.as_bytes()).unwrap(),
            vec![text_delta("hi")]
        );
    }

    #[test]
    fn decoder_ignores_comment_and_done_frames() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.push(b": keep-alive\n\ndata: [DONE]\n\n").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn decoder_finish_flushes_unterminated_trailing_event() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {CHUNK}");
        assert_eq!(decoder.push(frame.as_bytes()).unwrap(), Vec::new());
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
            ..AgentConfig::builtin()
        };
        let client = DeepSeekClient::new(config.clone()).expect("client builds");
        let request = ChatRequest::streaming(
            config.model.clone(),
            vec![crate::message::Message::user("Reply with exactly: pong")],
        );

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
    }
}
