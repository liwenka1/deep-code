#[cfg(test)]
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
#[cfg(test)]
use crate::session_store::SessionStoreError;
use crate::session_store::{JsonSessionStore, SessionId, SessionRecord, SessionStore};
use crate::tool::ToolRegistry;

fn build_persistence(store: JsonSessionStore, record: SessionRecord) -> Arc<Persistence> {
    let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(store);
    let record = Arc::new(Mutex::new(record));
    let actor = PersistenceActorHandle::spawn(store, Arc::clone(&record));
    Arc::new(Persistence { record, actor })
}

/// In-process session state for a configuration change. Only a live runtime
/// can create this value; disk records must use the authenticated resume path.
pub struct LiveSessionSnapshot {
    pub(crate) record: SessionRecord,
    pub(crate) persisted: bool,
    pub(crate) state: LiveSessionState,
}

pub(crate) struct LiveSessionState {
    session_approved: std::collections::HashSet<String>,
    session_trusted_shell_prefixes: std::collections::HashSet<(String, bool)>,
    cascade_escalated: bool,
    last_prefix: Option<(usize, u64)>,
    image_projections: super::state::ImageProjectionIndex,
}

impl AgentRuntime {
    /// Create a runtime backed by a new on-disk session in the workspace.
    ///
    /// `#[cfg(test)]`: no production path constructs a runtime this way — the
    /// launch code in `runtime_launch` builds the session record itself and
    /// goes through [`Self::from_session_record`], then attaches the shared
    /// write boundary. This shortcut exists for integration tests that need a
    /// disk-backed session without a full launch (and, like every raw
    /// constructor, it leaves `boundary: None`).
    #[cfg(test)]
    pub(crate) fn with_new_session<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        workspace: impl Into<PathBuf>,
        config: &AgentConfig,
    ) -> Result<Self, SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let mut record = SessionRecord::new(workspace.clone(), system);
        // First write is synchronous so callers see the session file before
        // returning. Subsequent saves go through the actor.
        store.save(&mut record)?;
        Ok(Self::from_session_record(
            client,
            tools,
            record,
            store,
            config.clone(),
        ))
    }

    /// Resume a runtime from a previously saved session record.
    ///
    /// `pub(crate)`: this constructor leaves `boundary: None`, so a runtime
    /// built from it alone cannot land a model-requested root grant — the
    /// launch paths in `runtime_launch` are the entries that follow up with
    /// `with_boundary` and hand out the same shared policy to the tool
    /// groups. Exposing this as a public "resume entry" invited exactly that
    /// trap: an external caller could not attach the boundary at all.
    ///
    /// `#[must_use]`: construction spawns the persistence actor — a dropped
    /// return value would leave that actor running against a session record
    /// nobody holds, with zero compiler noise.
    #[must_use]
    pub(crate) fn from_session_record<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        record: SessionRecord,
        store: JsonSessionStore,
        config: AgentConfig,
    ) -> Self {
        Self::from_record(client, tools, record, Some(store), config)
    }

    pub(crate) fn from_record<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        record: SessionRecord,
        store: Option<JsonSessionStore>,
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
                image_projections: super::state::ImageProjectionIndex::from_state(
                    &record.image_projection_state,
                ),
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
            compact_instructions: None,
            lsp: None,
            jobs: None,
            persistence: store.map(|store| build_persistence(store, record)),
            permission_mode: crate::execution_policy::SharedPermissionMode::default(),
            ui_lang,
        }
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        let persistence = self.persistence.as_ref()?;
        Some(persistence.record.lock().await.id.clone())
    }

    /// Capture the actual idle conversation without depending on a successful
    /// save, a readable session file, or an available signing key.
    pub async fn live_snapshot(&self) -> std::io::Result<LiveSessionSnapshot> {
        let workspace = self.workspace.clone().ok_or_else(|| {
            std::io::Error::other("cannot reconfigure a runtime without a workspace")
        })?;
        let record_guard = if let Some(persistence) = &self.persistence {
            Some(persistence.record.lock().await)
        } else {
            None
        };
        let state = self.state.lock().await;
        if state.current_turn_id.is_some() || state.pending.is_some() {
            return Err(std::io::Error::other("cannot reconfigure an active turn"));
        }
        let mut record = record_guard
            .as_deref()
            .cloned()
            .unwrap_or_else(|| SessionRecord::new(workspace, ""));
        record.entries = state.session.entries().to_vec();
        record.image_projection_state = state
            .image_projections
            .snapshot(&state.session.image_observation_ids());
        record.limit_image_reports();
        record.session_cost = state.session_cost;
        record.session_cache_hit_tokens = state.session_cache_hit_tokens;
        record.session_cache_miss_tokens = state.session_cache_miss_tokens;
        record.session_cache_savings = state.session_cache_savings;
        record.record_mac = None;
        Ok(LiveSessionSnapshot {
            record,
            persisted: self.persistence.is_some(),
            state: LiveSessionState {
                session_approved: state.session_approved.clone(),
                session_trusted_shell_prefixes: state.session_trusted_shell_prefixes.clone(),
                cascade_escalated: state.cascade_escalated,
                last_prefix: state.last_prefix,
                image_projections: state.image_projections.clone(),
            },
        })
    }

    pub(crate) fn with_live_state(self, live: Option<LiveSessionState>) -> Self {
        if let Some(live) = live {
            let mut state = self
                .state
                .try_lock()
                .expect("new runtime has no active tasks");
            state.session_approved = live.session_approved;
            state.session_trusted_shell_prefixes = live.session_trusted_shell_prefixes;
            state.cascade_escalated = live.cascade_escalated;
            state.last_prefix = live.last_prefix;
            state.image_projections = live.image_projections;
        }
        self
    }

    /// Failure of the latest completed save, cleared by a successful retry.
    /// Call after [`Self::shutdown`] to check the final durable snapshot.
    #[must_use]
    pub fn last_save_error(&self) -> Option<String> {
        self.persistence
            .as_ref()
            .and_then(|persistence| persistence.actor.last_save_error())
    }

    /// Persist the current session and wait for the writer's final outcome.
    pub async fn flush_session(&self) -> Option<String> {
        self.persist().await;
        if let Some(persistence) = &self.persistence {
            persistence.actor.flush().await;
        }
        self.last_save_error()
    }

    pub(super) async fn persist(&self) {
        self.persist_with(|_| {}).await;
    }

    /// [`Self::persist`] with a chance to amend the record under the same lock
    /// it is written under.
    ///
    /// Compaction needs this: its summary and archived count describe exactly
    /// the entry list saved with them, so one save has to carry all three. It
    /// also flushes the lifetime cost totals, which a hand-built record did
    /// not — the summarizer request is billed to the session like any other,
    /// and a `/compact` followed by a quit used to lose that spend.
    pub(super) async fn persist_with(&self, edit: impl FnOnce(&mut SessionRecord)) {
        let Some(persistence) = self.persistence.as_ref() else {
            let mut state = self.state.lock().await;
            let active = state.session.image_observation_ids();
            state.image_projections.retain_observations(&active);
            if let Some(turn) = &mut state.current_turn {
                let mut remaining = crate::session_store::IMAGE_REPORT_BUDGET_BYTES;
                turn.limit_image_reports(&mut remaining);
            }
            return;
        };
        {
            // Record first: waiting for disk serialization must not block turn
            // cancellation. The same lock order is used by live_snapshot.
            let mut record = persistence.record.lock().await;
            let mut state = self.state.lock().await;
            let active = state.session.image_observation_ids();
            state.image_projections.retain_observations(&active);
            record.entries = state.session.entries().to_vec();
            record.image_projection_state = state.image_projections.snapshot(&active);
            if let Some(turn) = state.current_turn.clone() {
                if let Some(existing) = record.turns.iter_mut().find(|entry| entry.id == turn.id) {
                    *existing = turn;
                } else {
                    record.turns.push(turn);
                }
            }
            record.session_cost = state.session_cost;
            record.session_cache_hit_tokens = state.session_cache_hit_tokens;
            record.session_cache_miss_tokens = state.session_cache_miss_tokens;
            record.session_cache_savings = state.session_cache_savings;
            edit(&mut record);
            record.limit_image_reports();
            // Keep the authority in sync, otherwise a later persist/finish
            // would restore the unbounded current-turn report list.
            if let Some(turn) = &mut state.current_turn
                && let Some(saved) = record.turns.iter().find(|saved| saved.id == turn.id)
            {
                *turn = saved.clone();
            }
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
            state.turn_events = None;
            drop(state);
            if let Some(persistence) = self.persistence.as_ref() {
                let mut record = persistence.record.lock().await;
                if let Some(existing) = record.turns.iter_mut().find(|entry| entry.id == turn.id) {
                    *existing = turn;
                } else {
                    record.turns.push(turn);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_turn_report_pruning_survives_repeated_persistence_and_finish() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let runtime = AgentRuntime::with_new_session(
            crate::echo_client::EchoClient::new(crate::i18n::SharedLang::default()),
            ToolRegistry::default(),
            "system",
            &workspace,
            &AgentConfig::builtin(),
        )
        .unwrap();
        let turn_id = runtime.begin_turn("observe").await;
        let asset = crate::image::snapshot_bytes(&workspace, b"GIF89a-observation").unwrap();
        let image = {
            let mut state = runtime.state.lock().await;
            state
                .session
                .push_user_with_images(turn_id.clone(), "steered image", vec![asset]);
            let ids = state.session.image_observation_ids();
            let image = crate::image::ImageDelivery {
                observation_id: Some(ids.iter().next().unwrap().clone()),
                message_index: 1,
                block_index: 1,
                call_id: None,
                image: "asset".into(),
                included: false,
                reason: Some("x".repeat(4096)),
            };
            let mut archived = image.clone();
            archived.observation_id = Some("archived-observation".into());
            state.image_projections.changes("flash", &[archived]);
            for index in 0..200 {
                let model = if index % 2 == 0 { "flash" } else { "pro" };
                state
                    .image_projections
                    .changes(model, std::slice::from_ref(&image));
                state.current_turn.as_mut().unwrap().image_requests.push(
                    crate::ImageRequestRecord {
                        model: model.into(),
                        images: vec![image.clone()],
                    },
                );
            }
            image
        };
        assert_eq!(runtime.flush_session().await, None);
        let bounded = runtime.state.lock().await.current_turn.clone().unwrap();
        assert!(bounded.image_reports_omitted > 0);
        assert!(bounded.image_requests.len() < 200);
        assert_eq!(runtime.flush_session().await, None);
        assert_eq!(
            runtime.state.lock().await.current_turn.as_ref(),
            Some(&bounded)
        );
        runtime.finish_turn(&turn_id).await;
        assert_eq!(runtime.flush_session().await, None);
        let snapshot = runtime.live_snapshot().await.unwrap();
        assert_eq!(snapshot.record.turns.last(), Some(&bounded));
        assert_eq!(snapshot.record.image_projection_state.len(), 1);
        assert_eq!(
            snapshot.record.image_projection_state[0].observation_id,
            image.observation_id.clone().unwrap()
        );
        assert_eq!(
            snapshot.record.image_projection_state[0].fingerprint.len(),
            64
        );
        let mut restored = super::super::state::ImageProjectionIndex::from_state(
            &snapshot.record.image_projection_state,
        );
        assert!(
            restored
                .changes("pro", std::slice::from_ref(&image))
                .is_empty()
        );
        assert_eq!(
            restored.changes("flash", std::slice::from_ref(&image)),
            vec![image]
        );
        let id = runtime.session_id().await.unwrap();
        runtime.shutdown().await;
        let saved = JsonSessionStore::for_workspace(&workspace)
            .unwrap()
            .load(&id)
            .unwrap();
        assert_eq!(saved.turns.last(), Some(&bounded));
        assert_eq!(
            saved.image_projection_state,
            snapshot.record.image_projection_state
        );
    }

    #[tokio::test]
    async fn live_reconfiguration_preserves_memory_only_session_state() {
        let workspace = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonicalize(workspace.path()).unwrap();
        let config = AgentConfig::builtin();
        let runtime = AgentRuntime::from_record(
            crate::echo_client::EchoClient::new(crate::i18n::SharedLang::default()),
            ToolRegistry::default(),
            SessionRecord::new(workspace.clone(), "system"),
            None,
            config.clone(),
        );
        {
            let mut state = runtime.state.lock().await;
            state.session_approved.insert("write_file".to_string());
            state
                .session_trusted_shell_prefixes
                .insert(("cargo test".to_string(), false));
            state.cascade_escalated = true;
            state.last_prefix = Some((4, 123));
            state.session_cost.usd = 0.25;
            state.session_cache_hit_tokens = 100;
        }
        let replacement = crate::runtime_launch::prepare_runtime_reconfiguration(
            &config,
            workspace.into(),
            runtime.live_snapshot().await.unwrap(),
        )
        .unwrap();
        let state = replacement.handle.state.lock().await;
        assert!(state.session_approved.contains("write_file"));
        assert!(
            state
                .session_trusted_shell_prefixes
                .contains(&("cargo test".to_string(), false))
        );
        assert!(state.cascade_escalated);
        assert_eq!(state.last_prefix, Some((4, 123)));
        assert_eq!(state.session_cost.usd, 0.25);
        assert_eq!(state.session_cache_hit_tokens, 100);
    }
}
