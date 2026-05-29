use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::checkpoint::CheckpointId;
use crate::event::AgentEvent;
use crate::model::Usage;
use crate::pricing::TurnTelemetry;
use crate::tool::{ApprovalRequest, ToolResult};

/// Events the agent runtime produces for UIs.
///
/// These are higher level than [`AgentEvent`]: approval requests and tool
/// results are emitted by the runtime, never by an [`crate::client::LlmClient`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// Forwarded provider event (text/reasoning/tool-call-delta).
    Provider(AgentEvent),
    /// Runtime is requesting human approval for a tool call.
    ApprovalRequired { request: ApprovalRequest },
    /// A tool finished (executed, denied, or failed) and its result has been
    /// recorded in the session.
    ToolResult { result: ToolResult },
    /// Current turn finished cleanly (no further provider activity).
    TurnFinished {
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry: Option<TurnTelemetry>,
    },
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
    Error { message: String },
}

pub type RuntimeEventReceiver = mpsc::UnboundedReceiver<RuntimeEvent>;

pub(super) fn emit(tx: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = tx.send(event);
}
