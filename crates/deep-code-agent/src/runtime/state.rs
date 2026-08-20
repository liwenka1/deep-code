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
    /// This turn's cost/cache totals, accumulated request-by-request at each
    /// stream `Done` (a multi-tool turn makes several requests; pricing only
    /// the last one would drop the rest). Reset by `begin_turn`.
    pub(super) turn_cost: CostEstimate,
    pub(super) turn_cache_hit_tokens: u64,
    pub(super) turn_cache_miss_tokens: u64,
    /// Cumulative DeepSeek prompt-cache tokens this session (hit is ~50–120×
    /// cheaper than miss), for surfacing cache efficiency. Like
    /// `session_cost`, flushed into the session record on save and restored
    /// on resume (see `persist` / `from_session_record`).
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
    /// True only for the turn in which `cascade_escalated` flipped on, so
    /// telemetry can surface the escalation at the moment it triggers (the
    /// triggering turn still finishes on Flash). Reset at the start of each turn.
    pub(super) cascade_triggered_this_turn: bool,
    /// Boundary denials (writes refused by the granted-roots fence — tool-layer
    /// path rejections and sandboxed shell write denials) observed in the
    /// current turn; reset at the start of each turn. Counted apart from
    /// `turn_tool_errors` because the two classes need opposite responses: an
    /// ordinary failure is something a stronger model might fix (cascade), a
    /// boundary denial is something only the user can fix (`/add-dir`) — the
    /// turn loop trips a circuit breaker on this counter instead of retrying.
    pub(super) turn_boundary_denials: u32,
    /// The most recent boundary-denied path (from the tool call's `path`
    /// argument), when one was extractable — shell denials don't carry one.
    /// Used to make the breaker's guidance concrete ("/add-dir <this dir>").
    /// Reset at the start of each turn.
    pub(super) last_boundary_denial_path: Option<String>,
}

pub(super) struct Persistence {
    /// The to-be-persisted *snapshot* of the transcript — NOT the authority.
    ///
    /// `RuntimeState.session` is authoritative; `persist()` overwrites this
    /// record's `entries` wholesale from it. Do not mutate entries in place here
    /// (e.g. `Arc::make_mut(&mut record.entries[i])`): the next save replaces the
    /// whole vector, so the change is silently discarded. Mutate the session and
    /// let `persist()` propagate. Durable writes are funnelled through
    /// [`PersistenceActorHandle`].
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
    /// For a parked `request_write_root` only: the canonical directory that
    /// was resolved when the prompt was built — the exact path the human saw.
    /// The grant re-resolves the request on approval and must land on this
    /// value, or it refuses (see `apply_root_grant`): a symlink shuffled
    /// between prompt and approval cannot substitute the target.
    pub(super) root_grant_target: Option<std::path::PathBuf>,
}
