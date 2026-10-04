use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// One turn on the wire.
///
/// Serialization is hand-written (see `ContentField`, private) rather than
/// derived, because one field — `content` — has two legal shapes and which one we
/// emit depends on a *different* field. Everything else matches what the derive
/// used to emit, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Images on this turn, as [`ImageRef::Path`] until a request is assembled
    /// and [`crate::image::hydrate`] turns them into [`ImageRef::Url`].
    ///
    /// **User turns only.** DeepSeek answers a 400 for an image in a system or
    /// assistant message, so [`Message::user_with_images`] is the one
    /// constructor that fills this in.
    ///
    /// Deliberately *not* folded into [`Self::content`]. Everything that reads a
    /// message as text — compaction's token estimate and excerpt, the prefix
    /// fingerprint, the session's wire⇄entry conversion, the subagent's report
    /// reader — must keep seeing a plain `String`, so the polymorphism is
    /// confined to the serialized form.
    ///
    /// Serializing a [`ImageRef::Path`] is an error, not a silent drop: a path
    /// is not a URL, and reaching a request with one is a bug in the request
    /// assembly, which is the one place it should be loud.
    pub images: Vec<ImageRef>,
    /// DeepSeek thinking-mode replay payload for assistant turns.
    pub reasoning_content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCallPayload>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
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

    /// A user turn that carries images as well as text.
    ///
    /// An empty `images` is exactly [`Self::user`], so a caller deriving them
    /// from a session does not need to special-case the text-only turn.
    #[must_use]
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageRef>) -> Self {
        let mut message = Self::user(content);
        message.images = images;
        message
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
            images: Vec::new(),
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
            images: Vec::new(),
            reasoning_content: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire form
// ---------------------------------------------------------------------------

/// The serialized shape of `content`.
///
/// A bare string when the turn carries nothing but text — byte-identical to
/// what every pre-image build sent, so no plain request body grows a wrapper —
/// and the content-parts array only once an image is actually attached.
#[derive(Serialize)]
#[serde(untagged)]
enum ContentField<'a> {
    Text(&'a str),
    Parts(Vec<ContentPart<'a>>),
}

impl<'a> ContentField<'a> {
    /// Only ever called with resolved images — [`Message`]'s `Serialize` rejects
    /// an unresolved path before it gets here.
    fn new(text: &'a str, images: &'a [ImageRef]) -> Self {
        if images.is_empty() {
            return Self::Text(text);
        }
        // An empty text part is legal but pointless. The API accepts a parts
        // array with images alone, and the composer's chip means the text is
        // not empty in practice anyway.
        let mut parts = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            parts.push(ContentPart::Text { kind: "text", text });
        }
        parts.extend(images.iter().map(|image| match image {
            ImageRef::Url { url, detail } => ContentPart::Image {
                kind: "image_url",
                image_url: ImageUrlPart {
                    url,
                    detail: detail.map(ImageDetail::as_str),
                },
            },
            // Unreachable: `Message`'s `Serialize` rejects an unresolved path
            // before it builds any content, and `Path` has no other way onto the
            // wire. A `filter_map` would have dropped the image silently here,
            // which is the exact failure the guard exists to prevent — so this
            // is a panic, and a loud one, not a `None`.
            ImageRef::Path(path) => {
                unreachable!("unresolved image path reached the wire: {}", path.display())
            }
        }));
        Self::Parts(parts)
    }
}

/// One block of the content-parts array.
///
/// The `type` tag is a literal on each variant because the variant already
/// decides it. `untagged` here only suppresses a tag — it is not a guess: the
/// caller picks the variant, and struct serialization always succeeds, so there
/// is no trial-and-error the way there is on the deserialize side.
#[derive(Serialize)]
#[serde(untagged)]
enum ContentPart<'a> {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: &'a str,
    },
    Image {
        #[serde(rename = "type")]
        kind: &'static str,
        image_url: ImageUrlPart<'a>,
    },
}

#[derive(Serialize)]
struct ImageUrlPart<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'static str>,
}

/// The deserialization view. Mirrors the old derive's field set and defaults,
/// with `content` left untyped so the two legal shapes can be told apart
/// without an untagged enum (whose failure mode is one opaque "did not match
/// any variant" for every problem, and which buffers the whole value).
#[derive(Deserialize)]
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

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(unresolved) = self.images.iter().find_map(ImageRef::path) {
            return Err(serde::ser::Error::custom(format!(
                "image attachment was never resolved for the wire: {}",
                unresolved.display()
            )));
        }
        // The other half of the API's rule (`images are supported in user
        // messages only`). Enforced here, at the wire boundary, for the same
        // reason the unresolved-path check is: a construction mistake has to be
        // loud rather than become a 400 the user pays for.
        if !self.images.is_empty() && self.role != Role::User {
            return Err(serde::ser::Error::custom(format!(
                "images are only legal on a user message, not on {:?}",
                self.role
            )));
        }
        let len = 2
            + usize::from(self.reasoning_content.is_some())
            + usize::from(self.tool_call_id.is_some())
            + usize::from(!self.tool_calls.is_empty());
        let mut state = serializer.serialize_struct("Message", len)?;
        state.serialize_field("role", &self.role)?;
        state.serialize_field("content", &ContentField::new(&self.content, &self.images))?;
        // Field order and the skip-when-absent rule are what the derive used to
        // produce; a request body that differs only in key order is still a
        // different byte string to the provider's prefix cache.
        if let Some(reasoning) = &self.reasoning_content {
            state.serialize_field("reasoning_content", reasoning)?;
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            state.serialize_field("tool_call_id", tool_call_id)?;
        }
        if !self.tool_calls.is_empty() {
            state.serialize_field("tool_calls", &self.tool_calls)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = MessageWire::deserialize(deserializer)?;
        let (content, images) = match wire.content {
            serde_json::Value::String(text) => (text, Vec::new()),
            serde_json::Value::Array(parts) => split_parts(parts, wire.role),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "message content must be a string or an array of content parts, got {other}"
                )));
            }
        };
        Ok(Self {
            role: wire.role,
            content,
            images,
            reasoning_content: wire.reasoning_content,
            tool_call_id: wire.tool_call_id,
            tool_calls: wire.tool_calls,
        })
    }
}

/// Flatten a content-parts array into the text buffer plus the images.
///
/// Unknown part kinds are skipped rather than refused: a provider may add a
/// block we do not model — DeepSeek's own `file`, an OpenAI `input_audio` — and
/// failing the whole response over an unread block would turn a perfectly
/// readable answer into a parse error.
///
/// Image parts on a message that is not from the user are dropped, and their
/// *text* is kept. The API rejects an image anywhere but a user turn, so an
/// `assistant` block carrying one cannot be represented — and keeping it would
/// poison the session: the message round-trips through `Serialize` on the next
/// request and fails there instead, stranding every turn after it. A response is
/// not ours to refuse, so the unrepresentable part is the part that goes.
fn split_parts(parts: Vec<serde_json::Value>, role: Role) -> (String, Vec<ImageRef>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for part in parts {
        match part.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(chunk) = part.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(chunk);
                }
            }
            Some("image_url") if role == Role::User => {
                let image_url = part.get("image_url");
                if let Some(url) = image_url
                    .and_then(|image| image.get("url"))
                    .and_then(serde_json::Value::as_str)
                {
                    images.push(ImageRef::Url {
                        url: url.to_string(),
                        detail: image_url
                            .and_then(|image| image.get("detail"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(ImageDetail::parse),
                    });
                }
            }
            _ => {}
        }
    }
    (text, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_url(url: &str) -> ImageRef {
        ImageRef::Url {
            url: url.to_string(),
            detail: None,
        }
    }

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

    // -----------------------------------------------------------------------
    // images
    // -----------------------------------------------------------------------

    /// The property the whole side-field design exists for: a turn with no
    /// images is byte-for-byte what this crate sent before the field existed.
    /// Wrapping `content` in an array would change every request body and every
    /// provider prefix-cache hit for no benefit.
    #[test]
    fn a_turn_without_images_keeps_the_bare_string_content() {
        let before = serde_json::to_string(&Message::user("hello")).unwrap();
        let after = serde_json::to_string(&Message::user_with_images("hello", Vec::new())).unwrap();

        assert_eq!(before, after);
        assert_eq!(after, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn images_serialize_as_content_parts() {
        let message =
            Message::user_with_images("what is this", vec![data_url("data:image/png;base64,AAAA")]);
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "what is this");
        assert_eq!(json["content"][1]["type"], "image_url");
        assert_eq!(
            json["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert!(
            json["content"][1]["image_url"].get("detail").is_none(),
            "an unset detail is omitted, not sent as null"
        );
    }

    #[test]
    fn a_set_detail_rides_alongside_the_url() {
        let message = Message::user_with_images(
            "chart",
            vec![ImageRef::Url {
                url: "data:image/png;base64,AA".to_string(),
                detail: Some(ImageDetail::Low),
            }],
        );
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["content"][1]["image_url"]["detail"], "low");
    }

    /// A path is not a URL. Sending one would be a request the API cannot read,
    /// so the failure belongs here, where the missing `hydrate` call is.
    #[test]
    fn serializing_an_unresolved_path_is_an_error() {
        let message = Message::user_with_images(
            "look",
            vec![ImageRef::Path(std::path::PathBuf::from("/tmp/shot.png"))],
        );

        let error = serde_json::to_string(&message).expect_err("an unresolved path cannot be sent");

        assert!(error.to_string().contains("/tmp/shot.png"), "{error}");
        assert!(error.to_string().contains("never resolved"), "{error}");
    }

    #[test]
    fn several_images_keep_their_order_after_the_text() {
        let message = Message::user_with_images(
            "compare these",
            vec![
                data_url("data:image/png;base64,AA"),
                data_url("https://example.com/b.jpg"),
            ],
        );
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["content"].as_array().unwrap().len(), 3);
        assert_eq!(
            json["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA"
        );
        assert_eq!(
            json["content"][2]["image_url"]["url"],
            "https://example.com/b.jpg"
        );
    }

    /// A parts array carrying images alone is legal, and an empty text block
    /// would be a block the model has to read for nothing.
    #[test]
    fn an_image_only_turn_omits_the_text_part() {
        let message = Message::user_with_images("", vec![data_url("data:image/png;base64,AA")]);
        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["content"].as_array().unwrap().len(), 1);
        assert_eq!(json["content"][0]["type"], "image_url");
    }

    #[test]
    fn content_parts_deserialize_into_text_and_images() {
        let message: Message = serde_json::from_str(
            r#"{"role":"user","content":[
                {"type":"text","text":"what is this"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(message.role, Role::User);
        assert_eq!(message.content, "what is this");
        assert_eq!(message.images, vec![data_url("data:image/png;base64,AAAA")]);
    }

    #[test]
    fn an_incoming_detail_is_parsed() {
        let message: Message = serde_json::from_str(
            r#"{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.com/a.jpg","detail":"low"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(
            message.images,
            vec![ImageRef::Url {
                url: "https://example.com/a.jpg".to_string(),
                detail: Some(ImageDetail::Low),
            }]
        );
    }

    #[test]
    fn a_message_with_images_round_trips() {
        let original = Message::user_with_images(
            "compare these",
            vec![
                ImageRef::Url {
                    url: "data:image/png;base64,AA".to_string(),
                    detail: Some(ImageDetail::Low),
                },
                data_url("https://example.com/b.jpg"),
            ],
        );
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, original);
    }

    /// The API rejects an image anywhere but a user turn, so a response carrying
    /// one on an `assistant` block cannot be represented. Keeping it would poison
    /// the session — the message fails `Serialize` on the next request and every
    /// turn after it is stranded — so the unrepresentable part is the part that
    /// goes, and the text survives.
    #[test]
    fn an_image_on_a_non_user_message_is_dropped_not_kept() {
        let message: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[
                {"type":"text","text":"here you go"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content, "here you go");
        assert!(message.images.is_empty());
        // And it can therefore still be sent back.
        assert!(serde_json::to_string(&message).is_ok());
    }

    /// The other half of the same rule: a construction mistake must be loud
    /// rather than become a 400 the user pays for.
    #[test]
    fn serializing_images_on_a_non_user_message_is_an_error() {
        let mut message = Message::system("system");
        message.images = vec![data_url("data:image/png;base64,AA")];

        let error = serde_json::to_string(&message).expect_err("only user turns take images");

        assert!(
            error.to_string().contains("only legal on a user message"),
            "{error}"
        );
    }

    #[test]
    fn content_that_is_neither_a_string_nor_an_array_is_refused() {
        let error = serde_json::from_str::<Message>(r#"{"role":"user","content":42}"#)
            .expect_err("a number is not content");

        assert!(
            error.to_string().contains("array of content parts"),
            "the error should name both legal shapes: {error}"
        );
    }

    /// A response is not ours to refuse: an unmodelled block must cost that
    /// block, not the answer.
    #[test]
    fn unknown_content_part_kinds_are_ignored() {
        let message: Message = serde_json::from_str(
            r#"{"role":"user","content":[
                {"type":"text","text":"keep me"},
                {"type":"file","file_id":"file-api-xxxx"},
                {"type":"input_audio","input_audio":{"data":"...","format":"wav"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(message.content, "keep me");
        assert!(message.images.is_empty());
    }

    #[test]
    fn text_only_content_still_deserializes() {
        let message: Message =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();

        assert_eq!(message.content, "hello");
        assert!(message.images.is_empty());
    }

    #[test]
    fn deserializing_keeps_tool_calls_and_reasoning() {
        let message: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"ok","reasoning_content":"hmm",
                "tool_calls":[{"id":"call_1","type":"function",
                "function":{"name":"mock_echo","arguments":"{}"}}]}"#,
        )
        .unwrap();

        assert_eq!(message.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "mock_echo");
    }
}
