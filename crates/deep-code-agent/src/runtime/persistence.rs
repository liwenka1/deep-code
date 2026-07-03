use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::model::Usage;
use crate::pricing::CostEstimate;
use crate::runtime::AgentRuntime;
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

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Create a runtime backed by a new on-disk session in the workspace.
    pub fn with_new_session(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        workspace: impl Into<PathBuf>,
        config: &AgentConfig,
    ) -> Result<Self, SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let record = SessionRecord::new(workspace.clone(), config, system);
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
    pub fn from_session_record(
        client: C,
        tools: ToolRegistry,
        record: SessionRecord,
        store: JsonSessionStore,
        config: AgentConfig,
    ) -> Self {
        let workspace = record.workspace.clone();
        let session = Session::from_entries(record.entries.clone());
        Self {
            client: Arc::new(client),
            config,
            registry: Default::default(),
            is_subagent: false,
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                pending: None,
                current_turn: None,
                last_prefix_hash: None,
                session_cost: CostEstimate::default(),
                session_cache_hit_tokens: 0,
                session_cache_miss_tokens: 0,
                session_cache_savings: CostEstimate::default(),
                current_prompt: None,
                current_turn_id: None,
                cancel: tokio_util::sync::CancellationToken::new(),
                session_approved: Default::default(),
                session_trusted_shell_prefixes: Default::default(),
                cascade_escalated: false,
                turn_tool_errors: 0,
                cascade_triggered_this_turn: false,
            })),
            checkpoints: None,
            workspace: Some(workspace.clone()),
            lsp: None,
            persistence: Some(build_persistence(store, record)),
        }
    }

    /// Attach session persistence to an existing runtime, creating a new record.
    pub fn enable_persistence(
        mut self,
        workspace: impl Into<PathBuf>,
        config: &AgentConfig,
        system_prompt: impl Into<String>,
    ) -> Result<Self, SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let mut record = SessionRecord::new(workspace.clone(), config, system_prompt);
        {
            let state = self.state.try_lock().map_err(|_| SessionStoreError::Io {
                message: "runtime state is busy".to_string(),
            })?;
            record.entries = state.session.entries().to_vec();
        }
        store.save(&record)?;
        self.workspace = Some(workspace);
        self.persistence = Some(build_persistence(store, record));
        Ok(self)
    }

    #[must_use]
    pub fn persistence_enabled(&self) -> bool {
        self.persistence.is_some()
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        let persistence = self.persistence.as_ref()?;
        Some(persistence.record.lock().await.id.clone())
    }

    pub(super) async fn persist(&self) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let entries = self.state.lock().await.session.entries().to_vec();
        {
            let mut record = persistence.record.lock().await;
            record.entries = entries;
            record.touch();
        }
        persistence.actor.request_save();
    }

    pub(super) async fn finish_turn(&self, usage: Option<Usage>) {
        let mut state = self.state.lock().await;
        if let Some(mut turn) = state.current_turn.take() {
            state.current_prompt = None;
            state.current_turn_id = None;
            turn.finish(usage);
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

    pub(super) async fn abort_turn(&self) {
        self.finish_turn(None).await;
    }

    pub(super) async fn finalize_orphan_turn(&self) {
        let has_open = self.state.lock().await.current_turn.is_some();
        if has_open {
            self.finish_turn(None).await;
        }
    }
}
