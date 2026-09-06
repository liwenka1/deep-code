use serde::{Deserialize, Serialize};

use crate::model::ToolCallPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// The wire spelling (`rename_all = "lowercase"`), for callers that need a
    /// stable string without a serialization round-trip — a fingerprint must
    /// not change because a variant was renamed in Rust.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// DeepSeek thinking-mode replay payload for assistant turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallPayload>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// Build an assistant message that carries `tool_calls`. Required by the
    /// OpenAI/DeepSeek protocol whenever the assistant turn requested tools;
    /// the subsequent `role=tool` messages must reference these `id`s.
    #[must_use]
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallPayload>,
    ) -> Self {
        Self::assistant_turn(content, "", tool_calls)
    }

    /// Build an assistant turn message, preserving optional reasoning replay.
    #[must_use]
    pub fn assistant_turn(
        content: impl Into<String>,
        reasoning: impl Into<String>,
        tool_calls: Vec<ToolCallPayload>,
    ) -> Self {
        let reasoning = reasoning.into();
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning_content: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            tool_call_id: None,
            tool_calls,
        }
    }

    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            reasoning_content: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_as_str_matches_the_serde_spelling() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            assert_eq!(
                serde_json::to_value(role).unwrap(),
                serde_json::Value::String(role.as_str().to_string()),
                "{role:?}"
            );
        }
    }

    #[test]
    fn role_serializes_as_openai_compatible_lowercase() {
        let json = serde_json::to_string(&Message::user("hello")).unwrap();

        assert_eq!(json, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn tool_message_serializes_tool_call_id() {
        let json = serde_json::to_string(&Message::tool("call_1", "ok")).unwrap();

        assert_eq!(
            json,
            r#"{"role":"tool","content":"ok","tool_call_id":"call_1"}"#
        );
    }

    #[test]
    fn assistant_turn_serializes_reasoning_content() {
        let message = Message::assistant_turn("answer", "thinking", Vec::new());
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "answer");
        assert_eq!(json["reasoning_content"], "thinking");
    }

    #[test]
    fn assistant_with_tool_calls_serializes_protocol_payload() {
        use crate::model::{ToolCallFunctionPayload, ToolCallPayload};

        let message = Message::assistant_with_tool_calls(
            "",
            vec![ToolCallPayload {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunctionPayload {
                    name: "mock_echo".to_string(),
                    arguments: r#"{"message":"hi"}"#.to_string(),
                },
            }],
        );
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "");
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "mock_echo");
        assert_eq!(
            json["tool_calls"][0]["function"]["arguments"],
            r#"{"message":"hi"}"#
        );
    }
}
