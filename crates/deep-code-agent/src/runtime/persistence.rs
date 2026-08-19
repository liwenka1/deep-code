use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::runtime::AgentRuntime;
use crate::runtime::event::TurnId;
use crate::runtime::persistence_actor::PersistenceActorHandle;
use crate::runtime::state::{Persistence, RuntimeState};
use crate::session::Session;
use crate::session_store::{
    JsonSessionStore, SessionId, SessionRecord, SessionStore, SessionStoreError,
};
use crate::tool::ToolRegistry;

fn build_persistence(store: JsonSessionStore, record: SessionRecord) -> Arc<Persistence> {
    let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(store);
    let record = Arc::new(Mutex::new(record));
    let actor = PersistenceActorHandle::spawn(store, Arc::clone(&record));
    Arc::new(Persistence { record, actor })
}

impl AgentRuntime {
    /// Create a runtime backed by a new on-disk session in the workspace.
    pub fn with_new_session<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        workspace: impl Into<PathBuf>,
        config: &AgentConfig,
    ) -> Result<Self, SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let record = SessionRecord::new(workspace.clone(), system);
        // First write is synchronous so callers see the session file before
        // returning. Subsequent saves go through the actor.
        store.save(&record)?;
        Ok(Self::from_session_record(
            client,
            tools,
            record,
            store,
            config.clone(),
        ))
    }

    /// Resume a runtime from a previously saved session record.
    #[must_use]
    pub fn from_session_record<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        record: SessionRecord,
        store: JsonSessionStore,
        config: AgentConfig,
    ) -> Self {
        let workspace = record.workspace.clone();
        let session = Session::from_entries(record.entries.clone());
        let ui_lang = crate::i18n::SharedLang::new(crate::i18n::Lang::from_env(&config.language));
        Self {
            client: Arc::new(client),
            config,
            registry: Default::default(),
            is_subagent: false,
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                // Restore the lifetime cost totals so a resumed session keeps
                // counting from its saved total instead of resetting to zero.
                session_cost: record.session_cost,
                session_cache_hit_tokens: record.session_cache_hit_tokens,
                session_cache_miss_tokens: record.session_cache_miss_tokens,
                session_cache_savings: record.session_cache_savings,
                ..Default::default()
            })),
            checkpoints: None,
            workspace: Some(workspace.clone()),
            boundary: None,
            lsp: None,
            persistence: Some(build_persistence(store, record)),
            permission_mode: crate::execution_policy::SharedPermissionMode::default(),
            ui_lang,
        }
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        let persistence = self.persistence.as_ref()?;
        Some(persistence.record.lock().await.id.clone())
    }

    pub(super) async fn persist(&self) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let (entries, cost, cache_hit, cache_miss, savings) = {
            let state = self.state.lock().await;
            (
                state.session.entries().to_vec(),
                state.session_cost,
                state.session_cache_hit_tokens,
                state.session_cache_miss_tokens,
                state.session_cache_savings,
            )
        };
        {
            let mut record = persistence.record.lock().await;
            record.entries = entries;
            // Flush the lifetime cost totals so resume restores them.
            record.session_cost = cost;
            record.session_cache_hit_tokens = cache_hit;
            record.session_cache_miss_tokens = cache_miss;
            record.session_cache_savings = savings;
            record.touch();
        }
        persistence.actor.request_save();
    }

    /// Close `turn_id`'s record. Guarded: a superseded loop (its turn was
    /// cancelled by a newer `begin_turn`) finalizing late must not consume the
    /// new turn's state, so a mismatched id is a no-op.
    pub(super) async fn finish_turn(&self, turn_id: &TurnId) {
        let mut state = self.state.lock().await;
        if state.current_turn_id.as_ref() != Some(turn_id) {
            return;
        }
        if let Some(turn) = state.current_turn.take() {
            state.current_prompt = None;
            state.current_turn_id = None;
            drop(state);
            if let Some(persistence) = self.persistence.as_ref() {
                let mut record = persistence.record.lock().await;
                record.turns.push(turn);
                record.touch();
            }
        } else {
            drop(state);
        }
        self.persist().await;
    }

    pub(super) async fn abort_turn(&self, turn_id: &TurnId) {
        self.finish_turn(turn_id).await;
    }

    pub(super) async fn finalize_orphan_turn(&self) {
        let open = self.state.lock().await.current_turn_id.clone();
        if let Some(turn_id) = open {
            self.finish_turn(&turn_id).await;
        }
    }
}
