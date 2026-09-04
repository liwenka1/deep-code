use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use super::telemetry::TurnTelemetry;
use crate::checkpoint::CheckpointId;
use crate::model::Usage;
use crate::session_store::{SessionId, now_ms};
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
/// These are higher level than [`AgentEvent`](crate::AgentEvent): approval requests and tool
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
    ToolCallFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    /// The user approved a `request_write_root`: `path` (canonical) is a
    /// writable root for the rest of the session, enforcement already live.
    /// UIs use this to update their own boundary display/state.
    RootGranted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        path: String,
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
        /// Set while the most recent disk save failed; cleared on recovery.
        /// UIs must surface this — the transcript on screen is NOT durable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        save_error: Option<String>,
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
    /// Non-fatal runtime warning for the UI to surface. Never terminal;
    /// exists so library code never writes to stderr while a raw-mode
    /// terminal owns the screen.
    Warning { message: String },
    /// Runtime-level error. Terminal for the current turn: every emitter ends
    /// the turn right after sending it, and consumers rely on that (the TUI
    /// stops observing the stream, headless stops the run). A degradation the
    /// loop survives — a failed checkpoint snapshot, say — is a
    /// [`Self::Warning`], never this: an `Error` from a loop that keeps going
    /// is a turn nobody is watching.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        message: String,
    },
}

impl RuntimeEvent {
    /// The dotted, namespaced event kind for the wire envelope's `item.kind`
    /// (e.g. `turn.started`). Lives on the enum so adding a variant updates the
    /// kind here — right next to the variant — instead of in a separate match in
    /// the runtime crate. Distinct from the serde `type` tag (snake_case), which
    /// exists so a payload can round-trip back into this enum; the two spellings
    /// are deliberate and both owned here.
    ///
    /// Stable wire contract: the SSE (`/v1/prompt`) and headless `stream-json`
    /// consumers match on `item.kind`, and the test below pins every string —
    /// change one only together with those consumers, as a deliberate red test.
    #[must_use]
    pub fn wire_kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn.started",
            Self::AssistantDelta { .. } => "assistant.delta",
            Self::ReasoningDelta { .. } => "reasoning.delta",
            Self::ToolCallStarted { .. } => "tool.started",
            Self::ToolCallUpdated { .. } => "tool.updated",
            Self::ToolCallProgress { .. } => "tool.progress",
            Self::ApprovalRequired { .. } => "approval.required",
            Self::ApprovalResolved { .. } => "approval.resolved",
            Self::ToolCallFinished { .. } => "tool.finished",
            Self::RootGranted { .. } => "root.granted",
            Self::SessionUpdated { .. } => "session.updated",
            Self::TurnFinished { .. } => "turn.completed",
            Self::TurnCancelled { .. } => "turn.cancelled",
            Self::CheckpointCreated { .. } => "checkpoint.created",
            Self::WorkspaceRestored { .. } => "workspace.restored",
            Self::DiagnosticsUpdated { .. } => "diagnostics.updated",
            Self::CompactionApplied { .. } => "compaction.applied",
            Self::Warning { .. } => "warning",
            Self::Error { .. } => "error",
        }
    }

    /// The turn this event belongs to, for the wire envelope's `item.turn_id`.
    /// `None` for session-level events (a save, a compaction, a checkpoint, a
    /// warning) and for turn-scoped events whose turn was not known when they
    /// were emitted. The second per-variant table of the envelope, kept beside
    /// [`Self::wire_kind`] for the same reason: a new variant is placed here,
    /// next to its definition, not in a match in the runtime crate.
    #[must_use]
    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::TurnStarted { turn_id, .. }
            | Self::AssistantDelta { turn_id, .. }
            | Self::ReasoningDelta { turn_id, .. }
            | Self::ToolCallStarted { turn_id, .. }
            | Self::ToolCallUpdated { turn_id, .. }
            | Self::TurnFinished { turn_id, .. }
            | Self::TurnCancelled { turn_id } => Some(turn_id),
            Self::ApprovalRequired { turn_id, .. }
            | Self::ApprovalResolved { turn_id, .. }
            | Self::ToolCallProgress { turn_id, .. }
            | Self::ToolCallFinished { turn_id, .. }
            | Self::RootGranted { turn_id, .. }
            | Self::Error { turn_id, .. } => turn_id.as_ref(),
            Self::SessionUpdated { .. }
            | Self::CompactionApplied { .. }
            | Self::CheckpointCreated { .. }
            | Self::WorkspaceRestored { .. }
            | Self::DiagnosticsUpdated { .. }
            | Self::Warning { .. } => None,
        }
    }
}

pub type RuntimeEventReceiver = mpsc::UnboundedReceiver<RuntimeEvent>;

pub(super) fn emit(tx: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = tx.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::RiskLevel;
    use crate::tool::{ApprovalDecision, ApprovalRequest, ToolResult};

    /// Every variant's `item.kind` and `item.turn_id`, pinned one by one. The
    /// envelope consumers match on these strings, so a rename must show up as
    /// a red test here rather than as a silent wire change; before this table
    /// only three of the nineteen kinds were asserted anywhere. The exhaustive
    /// `match` in `wire_kind` already forces a string for a new variant — the
    /// length check below is the nudge to pin it here too.
    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "shell".to_string(),
            description: "run cargo test".to_string(),
            arguments: serde_json::json!({ "command": "cargo test" }),
            risk_level: RiskLevel::High,
            requires_sandbox: true,
            read_only: false,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        }
    }

    #[test]
    fn wire_kind_and_turn_id_are_pinned_for_every_variant() {
        let turn = TurnId("turn_1".to_string());
        let call = ToolCallId("call_1".to_string());
        let request = sample_request();
        let result = ToolResult::success("call_1", "shell", "ok");
        let some_turn = Some(&turn);
        let cases: Vec<(RuntimeEvent, &str, Option<&TurnId>)> = vec![
            (
                RuntimeEvent::TurnStarted {
                    turn_id: turn.clone(),
                    prompt: "hi".to_string(),
                },
                "turn.started",
                some_turn,
            ),
            (
                RuntimeEvent::AssistantDelta {
                    turn_id: turn.clone(),
                    text: "a".to_string(),
                },
                "assistant.delta",
                some_turn,
            ),
            (
                RuntimeEvent::ReasoningDelta {
                    turn_id: turn.clone(),
                    text: "r".to_string(),
                },
                "reasoning.delta",
                some_turn,
            ),
            (
                RuntimeEvent::ToolCallStarted {
                    turn_id: turn.clone(),
                    tool_call_id: call.clone(),
                    tool_name: "shell".to_string(),
                    arguments: serde_json::json!({}),
                },
                "tool.started",
                some_turn,
            ),
            (
                RuntimeEvent::ToolCallUpdated {
                    turn_id: turn.clone(),
                    tool_call_id: call.clone(),
                    arguments_delta: None,
                },
                "tool.updated",
                some_turn,
            ),
            (
                RuntimeEvent::ToolCallProgress {
                    turn_id: Some(turn.clone()),
                    tool_call_id: call.clone(),
                    tool_name: "shell".to_string(),
                    update: ToolUpdate {
                        text: "…".to_string(),
                        details: None,
                    },
                },
                "tool.progress",
                some_turn,
            ),
            (
                RuntimeEvent::ApprovalRequired {
                    turn_id: Some(turn.clone()),
                    tool_call_id: Some(call.clone()),
                    request: request.clone(),
                },
                "approval.required",
                some_turn,
            ),
            (
                RuntimeEvent::ApprovalResolved {
                    turn_id: Some(turn.clone()),
                    tool_call_id: call.clone(),
                    decision: ApprovalDecision::Approved,
                },
                "approval.resolved",
                some_turn,
            ),
            (
                RuntimeEvent::ToolCallFinished {
                    turn_id: Some(turn.clone()),
                    tool_call_id: call.clone(),
                    result,
                },
                "tool.finished",
                some_turn,
            ),
            (
                RuntimeEvent::RootGranted {
                    turn_id: Some(turn.clone()),
                    path: "/tmp/x".to_string(),
                },
                "root.granted",
                some_turn,
            ),
            (
                RuntimeEvent::SessionUpdated {
                    session_id: Some(SessionId("s1".to_string())),
                    current_turn_id: Some(turn.clone()),
                    message_count: 1,
                    turn_count: 1,
                    summary: None,
                    compaction: None,
                    save_error: None,
                    updated_at_ms: 0,
                },
                "session.updated",
                None,
            ),
            (
                RuntimeEvent::TurnFinished {
                    turn_id: turn.clone(),
                    usage: Some(Usage::default()),
                    telemetry: None,
                },
                "turn.completed",
                some_turn,
            ),
            (
                RuntimeEvent::TurnCancelled {
                    turn_id: turn.clone(),
                },
                "turn.cancelled",
                some_turn,
            ),
            (
                RuntimeEvent::CheckpointCreated {
                    id: CheckpointId("c1".to_string()),
                    label: "before turn".to_string(),
                },
                "checkpoint.created",
                None,
            ),
            (
                RuntimeEvent::WorkspaceRestored {
                    id: CheckpointId("c1".to_string()),
                },
                "workspace.restored",
                None,
            ),
            (
                RuntimeEvent::DiagnosticsUpdated {
                    summary: "1 error".to_string(),
                    rendered: "E0308".to_string(),
                },
                "diagnostics.updated",
                None,
            ),
            (
                RuntimeEvent::CompactionApplied {
                    archived_count: 2,
                    summary: "…".to_string(),
                },
                "compaction.applied",
                None,
            ),
            (
                RuntimeEvent::Warning {
                    message: "w".to_string(),
                },
                "warning",
                None,
            ),
            (
                RuntimeEvent::Error {
                    turn_id: Some(turn.clone()),
                    message: "e".to_string(),
                },
                "error",
                some_turn,
            ),
        ];
        assert_eq!(cases.len(), 19, "a new variant must be pinned here too");
        for (event, kind, turn_id) in &cases {
            assert_eq!(event.wire_kind(), *kind, "{event:?}");
            assert_eq!(event.turn_id(), *turn_id, "{event:?}");
        }
    }

    /// The variants whose turn id is optional pass `None` through as `None`.
    /// The table above pins their `Some` side, so an arm hard-wired to either
    /// answer fails one of the two tests — a `Some`-only table let a `=> None`
    /// arm for `approval.required` or `root.granted` survive unnoticed.
    #[test]
    fn optional_turn_ids_pass_none_through() {
        let call = ToolCallId("call_1".to_string());
        let events = [
            RuntimeEvent::ApprovalRequired {
                turn_id: None,
                tool_call_id: Some(call.clone()),
                request: sample_request(),
            },
            RuntimeEvent::ApprovalResolved {
                turn_id: None,
                tool_call_id: call.clone(),
                decision: ApprovalDecision::Approved,
            },
            RuntimeEvent::ToolCallProgress {
                turn_id: None,
                tool_call_id: call.clone(),
                tool_name: "shell".to_string(),
                update: ToolUpdate {
                    text: "…".to_string(),
                    details: None,
                },
            },
            RuntimeEvent::ToolCallFinished {
                turn_id: None,
                tool_call_id: call,
                result: ToolResult::success("call_1", "shell", "ok"),
            },
            RuntimeEvent::RootGranted {
                turn_id: None,
                path: "/tmp/x".to_string(),
            },
            RuntimeEvent::Error {
                turn_id: None,
                message: "e".to_string(),
            },
        ];
        for event in &events {
            assert_eq!(event.turn_id(), None, "{event:?}");
        }
    }
}
