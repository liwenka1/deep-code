//! SSE envelope construction for `/prompt`.
//!
//! The wire shape (`RuntimeEnvelope` → `RuntimeItem` → kind/payload) is a
//! shared format: consumers match on `.item.kind` and read
//! `.item.payload.*` with jq-style paths. Both per-variant tables — the kind
//! string and the turn id — live on `RuntimeEvent` itself (`wire_kind`,
//! `turn_id`), next to the variants, and every kind is pinned by a test there;
//! update consumers when changing the format. (The repository's own CI bot
//! is not one of them: it runs `--output-format json` and reads the final
//! report, not this stream.)
//!
//! The headless CLI (`deep-code -p --output-format stream-json`) emits the
//! same envelopes as NDJSON, one per line — deliberately, so a consumer's jq
//! paths work unchanged against either entry point.

use chrono::Utc;
use deep_code_agent::{RuntimeEvent, TurnId, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeItem {
    pub thread_id: String,
    pub turn_id: Option<TurnId>,
    pub item_id: String,
    pub seq: u64,
    pub kind: String,
    pub created_at_ms: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEnvelope {
    pub thread_id: String,
    pub seq: u64,
    pub timestamp: String,
    pub item: RuntimeItem,
}

/// Per-request envelope sequencer: wraps runtime events (or manual items)
/// into the wire envelope with a monotonically increasing `seq`.
///
/// Public because the SSE stream and the headless NDJSON stream must share
/// one sequencer implementation — two copies would drift into two contracts.
pub struct EnvelopeStream {
    thread_id: String,
    seq: u64,
}

impl EnvelopeStream {
    #[must_use]
    pub fn new(thread_id: String) -> Self {
        Self { thread_id, seq: 0 }
    }

    pub fn event(&mut self, event: &RuntimeEvent) -> RuntimeEnvelope {
        self.wrap(
            event.wire_kind().to_string(),
            event.turn_id().cloned(),
            event_payload(event),
        )
    }

    pub fn manual(&mut self, kind: impl Into<String>, payload: Value) -> RuntimeEnvelope {
        self.wrap(kind.into(), None, payload)
    }

    fn wrap(&mut self, kind: String, turn_id: Option<TurnId>, payload: Value) -> RuntimeEnvelope {
        self.seq += 1;
        let seq = self.seq;
        RuntimeEnvelope {
            thread_id: self.thread_id.clone(),
            seq,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            item: RuntimeItem {
                thread_id: self.thread_id.clone(),
                turn_id,
                item_id: format!("{}_item_{seq}", self.thread_id),
                seq,
                kind,
                created_at_ms: now_ms(),
                payload,
            },
        }
    }
}

#[must_use]
pub fn event_payload(event: &RuntimeEvent) -> Value {
    serde_json::to_value(event).expect("runtime events contain serializable observation data")
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::{
        Content, ContentBlock, ImageAsset, ImageDelivery, ImageFormat, ToolCallId, ToolResult,
    };
    use serde_json::json;

    #[test]
    fn tool_envelopes_preserve_ordered_content_and_nested_execution_streams() {
        let asset = ImageAsset {
            workspace: "/workspace".into(),
            digest: "a".repeat(64),
            format: ImageFormat::Png,
            bytes: 70,
        };
        let mut result = ToolResult::success(
            "call",
            "read_file",
            Content::new(vec![
                ContentBlock::Text {
                    text: "before".into(),
                },
                ContentBlock::Image {
                    image: asset.clone(),
                },
                ContentBlock::Text {
                    text: "after".into(),
                },
            ]),
        );
        result.details = Some(json!({
            "job_id": "job_1", "status": "failed", "exit_code": 1,
            "stdout": { "text": "out", "total_bytes": 3, "omitted_bytes": 0,
                "inline_truncated": false, "capture_complete": true, "finished": true,
                "read_error": null, "spill": null },
            "stderr": { "text": "err", "total_bytes": 9, "omitted_bytes": 6,
                "inline_truncated": true, "capture_complete": false, "finished": true,
                "read_error": "pipe failed", "spill": null }
        }));
        let event = RuntimeEvent::ToolCallFinished {
            turn_id: Some(TurnId("turn".into())),
            tool_call_id: ToolCallId("call".into()),
            result,
        };
        let mut stream = EnvelopeStream::new("thread".into());
        let envelope = stream.event(&event);
        assert_eq!(envelope.item.kind, "tool.finished");
        let payload = &envelope.item.payload["result"];
        assert_eq!(payload["content"][0]["text"], "before");
        assert_eq!(payload["content"][1]["image"]["digest"], asset.digest);
        assert_eq!(payload["content"][2]["text"], "after");
        assert_eq!(payload["details"]["stdout"]["text"], "out");
        assert_eq!(payload["details"]["stderr"]["read_error"], "pipe failed");
        assert!(payload["content"].is_array());
        assert!(!serde_json::to_string(&envelope).unwrap().contains("base64"));
        assert_eq!(
            serde_json::from_value::<RuntimeEvent>(envelope.item.payload).unwrap(),
            event
        );
    }

    #[test]
    fn image_projection_envelope_reports_omissions_without_image_payloads() {
        let event = RuntimeEvent::ImageRequestPrepared {
            turn_id: TurnId("turn".into()),
            model: "deepseek-v4-pro".into(),
            images: vec![ImageDelivery {
                observation_id: Some("fixture-observation".into()),
                message_index: 2,
                block_index: 1,
                call_id: Some("call".into()),
                image: ".deep-code/images/asset.png".into(),
                included: false,
                reason: Some("current model does not accept images".into()),
            }],
        };
        let envelope = EnvelopeStream::new("thread".into()).event(&event);
        assert_eq!(envelope.item.kind, "image.request_prepared");
        assert_eq!(envelope.item.payload["images"][0]["included"], false);
        assert_eq!(envelope.item.payload["images"][0]["call_id"], "call");
        assert!(envelope.item.payload["images"][0].get("url").is_none());
        assert!(!serde_json::to_string(&envelope).unwrap().contains("base64"));
    }
}
