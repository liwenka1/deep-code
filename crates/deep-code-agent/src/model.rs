use serde::{Deserialize, Serialize};

use crate::message::Message;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default = "default_stream")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
}

impl ChatRequest {
    #[must_use]
    pub fn streaming(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            stream: true,
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ChatTool>) -> Self {
        self.tools = tools;
        self
    }
}

fn default_stream() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: Option<Message>,
    pub delta: Option<ChoiceDelta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: Option<u32>,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ChatToolFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tool call payload as it appears on a finalized assistant message.
///
/// Distinct from [`ToolCallDelta`], which represents incremental streaming
/// fragments of the same logical call. The `arguments` field is kept as a raw
/// JSON string so that history sent back to the provider is byte-identical to
/// what the model originally produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunctionPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFunctionPayload {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChunk {
    pub id: Option<String>,
    pub model: Option<String>,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn chat_request_serializes_minimal_streaming_payload() {
        let request = ChatRequest::streaming("deepseek-v4-pro", vec![Message::user("ping")]);
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["model"], "deepseek-v4-pro");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "ping");
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn chat_request_serializes_openai_compatible_tools() {
        let request = ChatRequest::streaming("deepseek-v4-pro", vec![Message::user("ping")])
            .with_tools(vec![ChatTool {
                tool_type: "function".to_string(),
                function: ChatToolFunction {
                    name: "mock_echo".to_string(),
                    description: "Echo safely".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }]);
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "mock_echo");
        assert_eq!(json["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn stream_chunk_deserializes_text_delta() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"id":"1","model":"deepseek-v4-pro","choices":[{"index":0,"message":null,"delta":{"role":null,"content":"hi"},"finish_reason":null}],"usage":null}"#,
        )
        .unwrap();

        assert_eq!(
            chunk.choices[0].delta.as_ref().unwrap().content.as_deref(),
            Some("hi")
        );
    }
}
