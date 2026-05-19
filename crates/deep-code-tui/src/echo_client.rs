//! Offline echo backend implementing [`LlmClient`].
//!
//! Used when `DEEPSEEK_API_KEY` is missing so the TUI is still demoable.
//! The `/mock-tool <message>` prefix is recognized and turned into a
//! `mock_echo` tool call so the approval flow can be exercised without a
//! real model.

use async_stream::try_stream;
use deep_code_agent::{
    AgentEvent, AgentEventStream, AgentResult, ChatRequest, FunctionCallDelta, LlmClient, Message,
    MockEchoTool, Role, ToolCallDelta,
};

#[derive(Debug, Default, Clone)]
pub struct EchoClient;

impl EchoClient {
    pub const MODEL: &'static str = "echo-offline";
}

impl LlmClient for EchoClient {
    fn provider_name(&self) -> &'static str {
        "echo"
    }

    fn model(&self) -> &str {
        Self::MODEL
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        let prompt = last_user_prompt(&request).unwrap_or_default();
        let tool_result = last_tool_result(&request);

        let stream = try_stream! {
            if let Some(result) = tool_result {
                for token in format!("Tool completed: {result}").split_inclusive(' ') {
                    yield AgentEvent::TextDelta { text: token.to_string() };
                }
                yield AgentEvent::Done { usage: None };
                return;
            }

            if let Some(message) = prompt.strip_prefix("/mock-tool ") {
                let arguments = serde_json::json!({ "message": message }).to_string();
                yield AgentEvent::ToolCallDelta {
                    delta: ToolCallDelta {
                        index: Some(0),
                        id: Some("echo_call_1".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(FunctionCallDelta {
                            name: Some(MockEchoTool::NAME.to_string()),
                            arguments: Some(arguments),
                        }),
                    },
                };
                yield AgentEvent::Done { usage: None };
                return;
            }

            for token in format!("Echo: {prompt}").split_inclusive(' ') {
                yield AgentEvent::TextDelta { text: token.to_string() };
            }
            yield AgentEvent::Done { usage: None };
        };

        Ok(Box::pin(stream))
    }
}

fn last_user_prompt(request: &ChatRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message: &&Message| matches!(message.role, Role::User))
        .map(|message| message.content.clone())
}

fn last_tool_result(request: &ChatRequest) -> Option<String> {
    request
        .messages
        .last()
        .filter(|message| matches!(message.role, Role::Tool))
        .map(|message| message.content.clone())
}
