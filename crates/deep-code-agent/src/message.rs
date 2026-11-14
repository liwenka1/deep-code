//! DeepSeek Chat Completions messages. Domain observations are projected here
//! without changing text/image ordering or tool-call identity.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::content::{Content, ContentBlock};
use crate::image::{ImageDetail, ImageRef};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: Content<ImageRef>,
    pub reasoning_content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCallPayload>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: impl Into<Content<ImageRef>>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning_content: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
    #[must_use]
    pub fn system(content: impl Into<Content<ImageRef>>) -> Self {
        Self::new(Role::System, content)
    }
    #[must_use]
    pub fn user(content: impl Into<Content<ImageRef>>) -> Self {
        Self::new(Role::User, content)
    }
    #[must_use]
    pub fn user_content(content: Content<ImageRef>) -> Self {
        Self::user(content)
    }
    #[must_use]
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageRef>) -> Self {
        let mut content = Content::from_text(content);
        for image in images {
            content.push_image(image);
        }
        Self::user(content)
    }
    #[must_use]
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallPayload>,
    ) -> Self {
        Self::assistant_turn(content, "", tool_calls)
    }
    #[must_use]
    pub fn assistant_turn(
        content: impl Into<String>,
        reasoning: impl Into<String>,
        tool_calls: Vec<ToolCallPayload>,
    ) -> Self {
        let reasoning = reasoning.into();
        Self {
            role: Role::Assistant,
            content: Content::from_text(content),
            reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
            tool_call_id: None,
            tool_calls,
        }
    }
    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<Content<ImageRef>>) -> Self {
        let mut message = Self::new(Role::Tool, content);
        message.tool_call_id = Some(tool_call_id.into());
        message
    }
    #[must_use]
    pub fn tool_content(tool_call_id: impl Into<String>, content: Content<ImageRef>) -> Self {
        Self::tool(tool_call_id, content)
    }
}

#[must_use]
pub fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == Role::Assistant)
        .map(|message| message.content.text())
        .find(|text| !text.trim().is_empty())
        .map(|text| text.trim_end().to_owned())
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(String),
    Parts(Vec<WirePart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WirePart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    Image { image_url: WireImage<'a> },
}

#[derive(Serialize)]
struct WireImage<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'static str>,
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.role == Role::Assistant && self.content.has_images() {
            return Err(serde::ser::Error::custom(
                "assistant messages cannot contain images",
            ));
        }
        let content = if self.content.has_images() {
            let mut parts = Vec::with_capacity(self.content.blocks.len());
            for block in &self.content.blocks {
                match block {
                    ContentBlock::Text { text } => parts.push(WirePart::Text { text }),
                    ContentBlock::Image {
                        image: ImageRef::Url { url, detail },
                    } => parts.push(WirePart::Image {
                        image_url: WireImage {
                            url,
                            detail: detail.map(ImageDetail::as_str),
                        },
                    }),
                    ContentBlock::Image {
                        image: ImageRef::Asset(asset),
                    } => {
                        return Err(serde::ser::Error::custom(format!(
                            "image asset was not prepared for this request: {asset}"
                        )));
                    }
                }
            }
            WireContent::Parts(parts)
        } else {
            WireContent::Text(self.content.text())
        };
        let mut state = serializer.serialize_struct(
            "Message",
            2 + usize::from(self.reasoning_content.is_some())
                + usize::from(self.tool_call_id.is_some())
                + usize::from(!self.tool_calls.is_empty()),
        )?;
        state.serialize_field("role", &self.role)?;
        state.serialize_field("content", &content)?;
        if let Some(reasoning) = &self.reasoning_content {
            state.serialize_field("reasoning_content", reasoning)?;
        }
        if let Some(id) = &self.tool_call_id {
            state.serialize_field("tool_call_id", id)?;
        }
        if !self.tool_calls.is_empty() {
            state.serialize_field("tool_calls", &self.tool_calls)?;
        }
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageWire {
    role: Role,
    content: serde_json::Value,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallPayload>,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = MessageWire::deserialize(deserializer)?;
        let content = match wire.content {
            serde_json::Value::String(text) => Content::from_text(text),
            serde_json::Value::Null => Content::default(),
            serde_json::Value::Array(parts) => {
                let mut content = Content::default();
                for part in parts {
                    match part.get("type").and_then(serde_json::Value::as_str) {
                        Some("text") => {
                            let text = part
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    serde::de::Error::custom("text block requires a string")
                                })?;
                            content.blocks.push(ContentBlock::Text {
                                text: text.to_owned(),
                            });
                        }
                        Some("image_url") => {
                            if wire.role == Role::Assistant {
                                return Err(serde::de::Error::custom(
                                    "assistant messages cannot contain images",
                                ));
                            }
                            let image = part.get("image_url").ok_or_else(|| {
                                serde::de::Error::custom("image_url block requires image_url")
                            })?;
                            let url = image
                                .get("url")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    serde::de::Error::custom("image_url requires a url string")
                                })?;
                            let detail = match image.get("detail") {
                                None | Some(serde_json::Value::Null) => None,
                                Some(serde_json::Value::String(value)) => {
                                    Some(ImageDetail::parse(value).ok_or_else(|| {
                                        serde::de::Error::custom("unsupported image detail")
                                    })?)
                                }
                                _ => {
                                    return Err(serde::de::Error::custom(
                                        "image detail must be a string",
                                    ));
                                }
                            };
                            content.push_image(ImageRef::Url {
                                url: url.to_owned(),
                                detail,
                            });
                        }
                        _ => {
                            return Err(serde::de::Error::custom(
                                "unsupported message content block",
                            ));
                        }
                    }
                }
                content
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "message content must be text or content blocks",
                ));
            }
        };
        Ok(Self {
            role: wire.role,
            content,
            reasoning_content: wire.reasoning_content,
            tool_call_id: wire.tool_call_id,
            tool_calls: wire.tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mixed_content() -> Content<ImageRef> {
        Content::new(vec![
            ContentBlock::Text {
                text: "first".into(),
            },
            ContentBlock::Image {
                image: ImageRef::Url {
                    url: "data:image/png;base64,AAAA".into(),
                    detail: Some(ImageDetail::Original),
                },
            },
            ContentBlock::Text {
                text: "second".into(),
            },
        ])
    }

    #[test]
    fn tool_images_round_trip_with_call_identity_and_content_order() {
        let message = Message::tool_content("call-1", mixed_content());
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call-1");
        assert_eq!(wire["content"][0]["text"], "first");
        assert_eq!(wire["content"][1]["type"], "image_url");
        assert_eq!(wire["content"][2]["text"], "second");
        assert_eq!(serde_json::from_value::<Message>(wire).unwrap(), message);
    }

    #[test]
    fn system_and_user_images_are_supported_and_assistant_images_are_rejected() {
        for role in [Role::System, Role::User] {
            let message = Message::new(role, mixed_content());
            assert_eq!(
                serde_json::from_value::<Message>(serde_json::to_value(&message).unwrap()).unwrap(),
                message
            );
        }
        let message = Message::new(Role::Assistant, mixed_content());
        assert!(serde_json::to_value(message).is_err());
        assert!(serde_json::from_value::<Message>(json!({"role":"assistant", "content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]})).is_err());
    }

    #[test]
    fn text_requests_and_reasoning_replay_use_protocol_fields() {
        let mut message = Message::assistant_turn("answer", "reasoning", Vec::new());
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(
            wire,
            json!({"role":"assistant","content":"answer","reasoning_content":"reasoning"})
        );
        message.content = Content::default();
        assert!(last_assistant_text(&[message]).is_none());
    }

    #[test]
    fn unsupported_blocks_fail_instead_of_losing_observations() {
        assert!(
            serde_json::from_value::<Message>(
                json!({"role":"tool","tool_call_id":"x","content":[{"type":"audio"}]})
            )
            .is_err()
        );
    }
}
