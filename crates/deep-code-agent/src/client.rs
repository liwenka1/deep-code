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

        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = config.timeout {
            builder = builder.timeout(timeout);
        }

        Ok(Self {
            http: builder.build()?,
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

        let response = self
            .http
            .post(self.config.chat_completions_url())
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.require_api_key()?),
            )
            .header(CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let message = response.text().await.unwrap_or_else(|_| status.to_string());
            return Err(AgentError::Api { status, message });
        }

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut pending = String::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(bytes) = byte_stream.next().await {
                let bytes = bytes?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|err| AgentError::Parse(err.to_string()))?;
                pending.push_str(text);

                while let Some(newline) = pending.find('\n') {
                    let line = pending[..newline].trim_end_matches('\r').to_string();
                    pending = pending[newline + 1..].to_string();

                    for event in parse_sse_line(&line)? {
                        yield event;
                    }
                }
            }

            for event in parse_sse_line(pending.trim_end_matches('\r'))? {
                yield event;
            }
        };

        Ok(Box::pin(stream))
    }
}

pub(crate) fn parse_sse_line(line: &str) -> AgentResult<Vec<AgentEvent>> {
    let Some(data) = line.trim().strip_prefix("data:") else {
        return Ok(Vec::new());
    };

    let data = data.trim();
    if data.is_empty() {
        return Ok(Vec::new());
    }

    if data == "[DONE]" {
        return Ok(Vec::new());
    }

    let chunk: StreamChunk = serde_json::from_str(data)?;
    Ok(chunk_to_events(chunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_line_treats_done_marker_as_transport_terminator() {
        assert_eq!(parse_sse_line("data: [DONE]").unwrap(), Vec::new());
    }

    #[test]
    fn parse_sse_line_maps_json_chunk() {
        let events = parse_sse_line(
            r#"data: {"id":"1","model":"deepseek-v4-pro","choices":[{"index":0,"message":null,"delta":{"role":null,"content":"hi"},"finish_reason":null}],"usage":null}"#,
        )
        .unwrap();

        assert_eq!(
            events,
            vec![AgentEvent::TextDelta {
                text: "hi".to_string()
            }]
        );
    }
}
