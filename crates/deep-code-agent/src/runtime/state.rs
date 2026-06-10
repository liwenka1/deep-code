use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;

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
    pub(super) current_prompt: Option<String>,
    pub(super) current_turn_id: Option<TurnId>,
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
