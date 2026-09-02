//! SSE envelope construction for `/v1/prompt`.
//!
//! The wire shape (`RuntimeEnvelope` → `RuntimeItem` → kind/payload) is a
//! stable contract: CI consumers extract text with jq paths like
//! `.item.kind` / `.item.payload.text`. Change it only together with those
//! consumers (`.github/workflows/deepcode-bot.yml`).
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
            event_turn_id(event),
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
    serde_json::to_value(event).unwrap_or_else(|_| serde_json::json!({}))
}

fn event_turn_id(event: &RuntimeEvent) -> Option<TurnId> {
    match event {
        RuntimeEvent::TurnStarted { turn_id, .. }
        | RuntimeEvent::AssistantDelta { turn_id, .. }
        | RuntimeEvent::ReasoningDelta { turn_id, .. }
        | RuntimeEvent::ToolCallStarted { turn_id, .. }
        | RuntimeEvent::ToolCallUpdated { turn_id, .. }
        | RuntimeEvent::TurnFinished { turn_id, .. }
        | RuntimeEvent::TurnCancelled { turn_id } => Some(turn_id.clone()),
        RuntimeEvent::ApprovalRequired { turn_id, .. }
        | RuntimeEvent::ApprovalResolved { turn_id, .. }
        | RuntimeEvent::ToolCallProgress { turn_id, .. }
        | RuntimeEvent::ToolCallFinished { turn_id, .. }
        | RuntimeEvent::RootGranted { turn_id, .. }
        | RuntimeEvent::Error { turn_id, .. } => turn_id.clone(),
        RuntimeEvent::SessionUpdated { .. }
        | RuntimeEvent::CompactionApplied { .. }
        | RuntimeEvent::CheckpointCreated { .. }
        | RuntimeEvent::WorkspaceRestored { .. }
        | RuntimeEvent::DiagnosticsUpdated { .. }
        | RuntimeEvent::Warning { .. } => None,
    }
}
