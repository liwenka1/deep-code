//! Offline placeholder backend implementing [`LlmClient`].
//!
//! Used when `DEEPSEEK_API_KEY` is missing: the TUI still starts (so a
//! first-run user can type `/apikey` to connect), but every submission
//! returns a fixed setup hint instead of pretending to work.

use async_stream::try_stream;

use crate::client::{AgentEventStream, LlmClient};
use crate::error::AgentResult;
use crate::event::AgentEvent;
use crate::model::ChatRequest;

#[derive(Debug, Default, Clone)]
pub struct EchoClient;

impl EchoClient {
    pub const MODEL: &'static str = "echo-offline";
    pub const PROVIDER: &'static str = "echo";
    const HINT: &'static str = "未配置 DeepSeek API key：输入 /apikey sk-... 即刻接入，\
或设置环境变量 DEEPSEEK_API_KEY 后重新启动。";
}

impl LlmClient for EchoClient {
    fn provider_name(&self) -> &'static str {
        Self::PROVIDER
    }

    fn model(&self) -> &str {
        Self::MODEL
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
        let stream = try_stream! {
            yield AgentEvent::TextDelta { text: Self::HINT.to_string() };
            yield AgentEvent::Done { usage: None };
        };
        Ok(Box::pin(stream))
    }
}
