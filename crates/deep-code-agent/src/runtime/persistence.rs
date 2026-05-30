use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::model::Usage;
use crate::pricing::CostEstimate;
use crate::runtime::AgentRuntime;
use crate::runtime::state::{Persistence, RuntimeState};
use crate::session::Session;
use crate::session_store::{
    JsonSessionStore, SessionId, SessionRecord, SessionStore, SessionStoreError,
};
use crate::tool::ToolRegistry;

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
        let session = Session::from_messages(record.messages.clone());
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
                current_prompt: None,
            })),
            checkpoints: None,
            workspace: Some(workspace.clone()),
            lsp: None,
            persistence: Some(Arc::new(Persistence {
                store: Arc::new(store),
                record: Arc::new(Mutex::new(record)),
            })),
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
            record.messages = state.session.messages().to_vec();
        }
        store.save(&record)?;
        self.workspace = Some(workspace);
        self.persistence = Some(Arc::new(Persistence {
            store: Arc::new(store),
            record: Arc::new(Mutex::new(record)),
        }));
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
        let messages = self.state.lock().await.session.messages().to_vec();
        let mut record = persistence.record.lock().await;
        record.messages = messages;
        record.touch();
        if let Err(error) = persistence.store.save(&record) {
            eprintln!("session save failed: {error}");
        }
    }

    pub(super) async fn finish_turn(&self, usage: Option<Usage>) {
        let mut state = self.state.lock().await;
        if let Some(mut turn) = state.current_turn.take() {
            turn.finish(usage);
            drop(state);
            if let Some(persistence) = self.persistence.as_ref() {
                let mut record = persistence.record.lock().await;
                record.turns.push(turn);
                record.touch();
                if let Err(error) = persistence.store.save(&record) {
                    eprintln!("session save failed: {error}");
                }
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
