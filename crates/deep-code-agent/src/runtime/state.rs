use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::event::TurnId;
use super::persistence_actor::PersistenceActorHandle;
use crate::pricing::CostEstimate;
use crate::session::Session;
use crate::session_store::{SessionRecord, TurnRecord};
use crate::tool::ToolCall;

/// Internal: the runtime can be in one of these states between
/// `RuntimeEvent` emissions. Kept crate-private to the runtime module so
/// callers cannot poke at it.
#[derive(Debug, Default)]
pub(super) struct RuntimeState {
    pub(super) session: Session,
    pub(super) pending: Option<PendingToolBatch>,
    pub(super) current_turn: Option<TurnRecord>,
    pub(super) last_prefix_hash: Option<u64>,
    pub(super) session_cost: CostEstimate,
    /// Cumulative DeepSeek prompt-cache tokens this session (hit is ~50–120×
    /// cheaper than miss), for surfacing cache efficiency. In-memory only.
    pub(super) session_cache_hit_tokens: u64,
    pub(super) session_cache_miss_tokens: u64,
    /// Cumulative spend avoided by cache hits this session (vs all-miss).
    pub(super) session_cache_savings: CostEstimate,
    pub(super) current_prompt: Option<String>,
    pub(super) current_turn_id: Option<TurnId>,
    /// Cancellation token for the in-flight turn; rotated by `begin_turn`.
    pub(super) cancel: CancellationToken,
    /// Tools the user approved for the whole session ("a" in the approval
    /// panel). In-memory only: forgotten when the runtime shuts down.
    pub(super) session_approved: HashSet<String>,
    /// Leading programs of shell commands the user approved for the session
    /// ("a" on a shell call) — e.g. `cargo`, `git`. Shell isn't blanket
    /// session-approvable by tool name, so this trusts at command granularity.
    /// In-memory only; compound commands are never matched (they keep prompting).
    pub(super) session_trusted_shell_prefixes: HashSet<String>,
    /// Cascade routing latch: set once Flash visibly struggles (repeated
    /// tool-call execution failures within a turn). Sticky for the rest of the
    /// session, forcing auto mode onto Pro. In-memory only.
    pub(super) cascade_escalated: bool,
    /// Tool-call execution failures observed in the current turn; reset at the
    /// start of each turn. Crossing the cascade threshold latches `cascade_escalated`.
    pub(super) turn_tool_errors: u32,
}

pub(super) struct Persistence {
    /// Authoritative in-memory transcript. Callers mutate this under the
    /// mutex; durable writes are funnelled through [`PersistenceActorHandle`].
    pub(super) record: Arc<Mutex<SessionRecord>>,
    pub(super) actor: PersistenceActorHandle,
}

/// A turn's tool calls awaiting approval: the call currently pending plus the
/// rest of the batch, resumed in order once the decision arrives.
#[derive(Debug, Clone)]
pub(super) struct PendingToolBatch {
    pub(super) current: ToolCall,
    pub(super) remaining: VecDeque<ToolCall>,
    pub(super) turn_id: TurnId,
}
