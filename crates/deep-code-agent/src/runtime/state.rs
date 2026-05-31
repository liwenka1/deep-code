use std::sync::Arc;

use tokio::sync::Mutex;

use super::event::TurnId;
use crate::pricing::CostEstimate;
use crate::session::Session;
use crate::session_store::{JsonSessionStore, SessionRecord, TurnRecord};
use crate::tool::ToolCall;

/// Internal: the runtime can be in one of these states between
/// `RuntimeEvent` emissions. Kept crate-private to the runtime module so
/// callers cannot poke at it.
#[derive(Debug, Default)]
pub(super) struct RuntimeState {
    pub(super) session: Session,
    pub(super) pending: Option<PendingToolCall>,
    pub(super) current_turn: Option<TurnRecord>,
    pub(super) last_prefix_hash: Option<u64>,
    pub(super) session_cost: CostEstimate,
    pub(super) current_prompt: Option<String>,
    pub(super) current_turn_id: Option<TurnId>,
}

pub(super) struct Persistence {
    pub(super) store: Arc<JsonSessionStore>,
    pub(super) record: Arc<Mutex<SessionRecord>>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingToolCall {
    pub(super) call: ToolCall,
    pub(super) turn_id: TurnId,
}
