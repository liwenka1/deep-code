use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::checkpoint::CheckpointId;
use crate::event::AgentEvent;
use crate::model::Usage;
use crate::pricing::TurnTelemetry;
use crate::session_store::SessionId;
use crate::tool::{ApprovalRequest, ToolResult, ToolUpdate};

/// Stable identifier for one user-visible agent turn.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

impl TurnId {
    #[must_use]
    pub fn new() -> Self {
        static TURN_COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("turn_{}_{seq}", now_ms()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identifier for a model-requested tool call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ToolCallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolCallId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Events the agent runtime produces for UIs.
///
/// These are higher level than [`AgentEvent`]: approval requests and tool
/// results are emitted by the runtime, never by an [`crate::client::LlmClient`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// A user-visible turn has started.
    TurnStarted { turn_id: TurnId, prompt: String },
    /// Assistant text suitable for transcript rendering.
    AssistantDelta { turn_id: TurnId, text: String },
    /// Reasoning text suitable for transcript rendering.
    ReasoningDelta { turn_id: TurnId, text: String },
    /// A parsed tool call is ready to evaluate or execute.
    ToolCallStarted {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: String,
        arguments: Value,
    },
    /// A provider streamed part of a tool call.
    ToolCallUpdated {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_delta: Option<String>,
    },
    /// Forwarded provider event (text/reasoning/tool-call-delta).
    Provider(AgentEvent),
    /// Runtime is requesting human approval for a tool call.
    ApprovalRequired {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
        request: ApprovalRequest,
    },
    /// A human approval request was resolved.
    ApprovalResolved {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        tool_call_id: ToolCallId,
        decision: crate::tool::ApprovalDecision,
    },
    /// Incremental progress from an executing tool (e.g. streamed shell
    /// output). Distinct from [`RuntimeEvent::ToolCallUpdated`], which is
    /// provider-side argument streaming.
    ToolCallProgress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        tool_call_id: ToolCallId,
        tool_name: String,
        update: ToolUpdate,
    },
    /// A tool finished (executed, denied, or failed) and its result has been
    /// recorded in the session.
    ToolResult { result: ToolResult },
    /// Structured tool completion event with stable ids.
    ToolCallFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    /// The authoritative session transcript changed.
    SessionUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_turn_id: Option<TurnId>,
        message_count: usize,
        turn_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction: Option<String>,
        updated_at_ms: u64,
    },
    /// Current turn finished cleanly (no further provider activity).
    TurnFinished {
        turn_id: TurnId,
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry: Option<TurnTelemetry>,
    },
    /// Current turn was cancelled by the user. Terminal for the turn; any
    /// unfinished tool calls already have synthesized error results recorded.
    TurnCancelled { turn_id: TurnId },
    /// Transcript compaction was applied before the model request.
    CompactionApplied {
        archived_count: usize,
        summary: String,
    },
    /// Workspace snapshot stored under `.deep-code/checkpoints/`.
    CheckpointCreated { id: CheckpointId, label: String },
    /// Workspace restored from a checkpoint (via runtime or UI command).
    WorkspaceRestored { id: CheckpointId },
    /// Post-edit LSP diagnostics were collected for one or more files.
    DiagnosticsUpdated { summary: String, rendered: String },
    /// Runtime-level error. Terminal for the current turn.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        message: String,
    },
}

pub type RuntimeEventReceiver = mpsc::UnboundedReceiver<RuntimeEvent>;

pub(super) fn emit(tx: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = tx.send(event);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}
