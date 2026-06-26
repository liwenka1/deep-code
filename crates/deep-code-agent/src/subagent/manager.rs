use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::handle::{HandleKind, HandleStore, VarHandle};
use crate::subagent::output::parse_structured_report;
use crate::subagent::types::{
    HARD_MAX_CONCURRENT, SUBAGENT_STATE_FILE, SUBAGENT_STATE_SCHEMA_VERSION, SubAgentError,
    SubAgentRecord, SubAgentSessionProjection, SubAgentStatus,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSubAgentState {
    schema_version: u32,
    session_boot_id: String,
    agents: Vec<SubAgentRecord>,
}

pub struct SubAgentManager {
    _workspace: PathBuf,
    max_concurrent: usize,
    pub(crate) session_boot_id: String,
    agents: std::collections::HashMap<String, SubAgentRecord>,
    handle_store: Arc<RwLock<HandleStore>>,
    state_path: PathBuf,
}

impl SubAgentManager {
    pub fn new(
        workspace: PathBuf,
        max_concurrent: usize,
        handle_store: Arc<RwLock<HandleStore>>,
    ) -> Self {
        let max_concurrent = max_concurrent.clamp(1, HARD_MAX_CONCURRENT);
        let state_path = default_state_path(&workspace);
        let mut manager = Self {
            _workspace: workspace,
            max_concurrent,
            session_boot_id: new_boot_id(),
            agents: std::collections::HashMap::new(),
            handle_store,
            state_path,
        };
        if let Err(error) = manager.load_state() {
            eprintln!("sub-agent state load failed: {error}");
        }
        manager
    }

    #[must_use]
    pub fn set_max_concurrent(&mut self, max_concurrent: usize) -> &mut Self {
        self.max_concurrent = max_concurrent.clamp(1, HARD_MAX_CONCURRENT);
        self
    }

    pub fn running_count(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| {
                agent.status == SubAgentStatus::Running
                    && agent.session_boot_id.as_deref() == Some(self.session_boot_id.as_str())
            })
            .count()
    }

    pub fn list_current_session(&self) -> Vec<SubAgentRecord> {
        let mut agents: Vec<_> = self
            .agents
            .values()
            .filter(|agent| agent.session_boot_id.as_deref() == Some(self.session_boot_id.as_str()))
            .cloned()
            .collect();
        agents.sort_by_key(|agent| std::cmp::Reverse(agent.started_at_ms));
        agents
    }

    pub fn get(&self, agent_id: &str) -> Option<&SubAgentRecord> {
        self.agents.get(agent_id)
    }

    pub fn insert(&mut self, record: SubAgentRecord) -> Result<(), SubAgentError> {
        if record.status == SubAgentStatus::Running
            && record.session_boot_id.as_deref() == Some(self.session_boot_id.as_str())
            && self.running_count() >= self.max_concurrent
        {
            return Err(SubAgentError::ConcurrencyLimit {
                cap: self.max_concurrent,
            });
        }
        self.agents.insert(record.agent_id.clone(), record);
        self.persist_state()
    }

    pub fn update(
        &mut self,
        agent_id: &str,
        update: impl FnOnce(&mut SubAgentRecord),
    ) -> Result<(), SubAgentError> {
        let record = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| SubAgentError::NotFound {
                id: agent_id.to_string(),
            })?;
        update(record);
        self.persist_state()
    }

    pub fn mark_cancelled(&mut self, agent_id: &str) -> Result<SubAgentRecord, SubAgentError> {
        self.update(agent_id, |record| {
            if !record.status.is_terminal() {
                record.status = SubAgentStatus::Cancelled;
                record.finished_at_ms = Some(now_ms());
                record.error = Some("cancelled by parent".to_string());
            }
        })?;
        self.get(agent_id)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound {
                id: agent_id.to_string(),
            })
    }

    pub fn project(
        &self,
        record: &SubAgentRecord,
        timed_out: bool,
    ) -> Result<SubAgentSessionProjection, SubAgentError> {
        let transcript_handle = record
            .transcript_handle
            .as_ref()
            .and_then(|id| {
                self.handle_store
                    .read()
                    .ok()
                    .and_then(|store| store.get_summary(id))
            })
            .map(|summary| VarHandle::from_summary(&summary, &record.name))
            .ok_or_else(|| SubAgentError::State {
                message: format!("missing transcript handle for {}", record.agent_id),
            })?;

        Ok(SubAgentSessionProjection {
            name: record.name.clone(),
            agent_id: record.agent_id.clone(),
            status: record.status.as_str().to_string(),
            terminal: record.status.is_terminal(),
            context_mode: if record.fork_context {
                "forked".to_string()
            } else {
                "fresh".to_string()
            },
            fork_context: record.fork_context,
            transcript_handle,
            snapshot: record.clone(),
            timed_out,
        })
    }

    pub fn store_transcript(
        &self,
        record: &SubAgentRecord,
    ) -> Result<crate::handle::HandleSummary, SubAgentError> {
        let payload = serde_json::json!({
            "kind": "subagent_session_snapshot",
            "agent_id": record.agent_id,
            "name": record.name,
            "status": record.status.as_str(),
            "assignment": record.assignment,
            "result": record.result,
            "structured": record.structured,
            "steps_taken": record.steps_taken,
            "started_at_ms": record.started_at_ms,
            "finished_at_ms": record.finished_at_ms,
        });
        let mut store = self
            .handle_store
            .write()
            .map_err(|error| SubAgentError::State {
                message: error.to_string(),
            })?;
        Ok(store.insert_json_with_owner(
            format!("agent:{}", record.agent_id),
            HandleKind::Transcript,
            payload,
            Some(record.name.clone()),
        ))
    }

    pub fn release_transcript_handles(
        &mut self,
        record: &SubAgentRecord,
    ) -> Result<(), SubAgentError> {
        if let Ok(mut store) = self.handle_store.write() {
            store.purge_session(&record.name);
        }
        if record.transcript_handle.is_some() {
            self.update(&record.agent_id, |record| {
                record.transcript_handle = None;
            })?;
        }
        Ok(())
    }

    pub fn finalize_success(
        &mut self,
        agent_id: &str,
        result_text: String,
        steps_taken: u32,
    ) -> Result<SubAgentRecord, SubAgentError> {
        let structured = parse_structured_report(&result_text);
        self.update(agent_id, |record| {
            record.result = Some(result_text);
            record.structured = structured;
            record.steps_taken = steps_taken;
            record.status = SubAgentStatus::Completed;
            record.finished_at_ms = Some(now_ms());
        })?;
        let record = self.get(agent_id).cloned().expect("record exists");
        let handle = self.store_transcript(&record)?;
        self.update(agent_id, |record| {
            record.transcript_handle = Some(handle.id);
        })?;
        self.get(agent_id)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound {
                id: agent_id.to_string(),
            })
    }

    pub fn finalize_failure(
        &mut self,
        agent_id: &str,
        message: String,
        steps_taken: u32,
    ) -> Result<SubAgentRecord, SubAgentError> {
        self.update(agent_id, |record| {
            record.error = Some(message.clone());
            record.steps_taken = steps_taken;
            record.status = SubAgentStatus::Failed;
            record.finished_at_ms = Some(now_ms());
        })?;
        let record = self.get(agent_id).cloned().expect("record exists");
        let handle = self.store_transcript(&record)?;
        self.update(agent_id, |record| {
            record.transcript_handle = Some(handle.id);
        })?;
        self.get(agent_id)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound {
                id: agent_id.to_string(),
            })
    }

    pub fn cancel_all(&mut self) {
        for record in self.agents.values_mut() {
            if record.status == SubAgentStatus::Running
                && record.session_boot_id.as_deref() == Some(self.session_boot_id.as_str())
            {
                record.status = SubAgentStatus::Cancelled;
                record.finished_at_ms = Some(now_ms());
                record.error = Some("parent session cancelled".to_string());
            }
        }
        let _ = self.persist_state();
    }

    fn load_state(&mut self) -> Result<(), SubAgentError> {
        if !self.state_path.exists() {
            return Ok(());
        }
        let payload =
            std::fs::read_to_string(&self.state_path).map_err(|error| SubAgentError::Io {
                message: error.to_string(),
            })?;
        let state: PersistedSubAgentState =
            serde_json::from_str(&payload).map_err(|error| SubAgentError::State {
                message: error.to_string(),
            })?;
        if state.schema_version != SUBAGENT_STATE_SCHEMA_VERSION {
            return Err(SubAgentError::State {
                message: format!(
                    "unsupported sub-agent schema {} (expected {SUBAGENT_STATE_SCHEMA_VERSION})",
                    state.schema_version
                ),
            });
        }
        for mut record in state.agents {
            if record.status == SubAgentStatus::Running {
                record.status = SubAgentStatus::Interrupted;
                record.error = Some("interrupted by process restart".to_string());
                record.finished_at_ms = Some(now_ms());
            }
            self.agents.insert(record.agent_id.clone(), record);
        }
        Ok(())
    }

    fn persist_state(&self) -> Result<(), SubAgentError> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| SubAgentError::Io {
                message: error.to_string(),
            })?;
        }
        let state = PersistedSubAgentState {
            schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
            session_boot_id: self.session_boot_id.clone(),
            agents: self.agents.values().cloned().collect(),
        };
        let payload =
            serde_json::to_string_pretty(&state).map_err(|error| SubAgentError::State {
                message: error.to_string(),
            })?;
        let tmp = self.state_path.with_extension("tmp");
        std::fs::write(&tmp, payload).map_err(|error| SubAgentError::Io {
            message: error.to_string(),
        })?;
        std::fs::rename(&tmp, &self.state_path).map_err(|error| SubAgentError::Io {
            message: error.to_string(),
        })?;
        Ok(())
    }
}

pub fn default_state_path(workspace: &Path) -> PathBuf {
    workspace
        .join(".deep-code")
        .join("state")
        .join(SUBAGENT_STATE_FILE)
}

pub fn new_agent_id() -> String {
    format!("agent_{}", uuid::Uuid::new_v4())
}

fn new_boot_id() -> String {
    format!("boot_{}", uuid::Uuid::new_v4())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_limit_blocks_running_agents() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(HandleStore::new()));
        let mut manager = SubAgentManager::new(dir.path().to_path_buf(), 1, store);
        let boot = manager.session_boot_id.clone();
        let first = SubAgentRecord {
            schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
            agent_id: "a1".to_string(),
            name: "first".to_string(),
            role: "explore".to_string(),
            status: SubAgentStatus::Running,
            assignment: "task".to_string(),
            result: None,
            structured: None,
            transcript_handle: None,
            error: None,
            fork_context: false,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            steps_taken: 0,
            session_boot_id: Some(boot.clone()),
        };
        manager.insert(first).unwrap();
        let second = SubAgentRecord {
            agent_id: "a2".to_string(),
            name: "second".to_string(),
            ..manager.get("a1").unwrap().clone()
        };
        assert!(matches!(
            manager.insert(second),
            Err(SubAgentError::ConcurrencyLimit { .. })
        ));
    }

    #[test]
    fn finalize_success_parses_structured_report() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(HandleStore::new()));
        let mut manager = SubAgentManager::new(
            dir.path().to_path_buf(),
            crate::subagent::types::DEFAULT_MAX_CONCURRENT,
            store,
        );
        let boot = manager.session_boot_id.clone();
        manager
            .insert(SubAgentRecord {
                schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
                agent_id: "a1".to_string(),
                name: "worker".to_string(),
                role: "general".to_string(),
                status: SubAgentStatus::Running,
                assignment: "do".to_string(),
                result: None,
                structured: None,
                transcript_handle: None,
                error: None,
                fork_context: false,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                steps_taken: 0,
                session_boot_id: Some(boot),
            })
            .unwrap();
        let text = "### SUMMARY\nDone.\n\n### EVIDENCE\nNone.\n\n### CHANGES\nNone.\n\n### RISKS\nNone observed.\n\n### BLOCKERS\nNone.\n";
        let record = manager.finalize_success("a1", text.to_string(), 1).unwrap();
        assert_eq!(record.status, SubAgentStatus::Completed);
        assert!(record.transcript_handle.is_some());
        assert!(record.structured.is_some());
        let projection = manager.project(&record, false).unwrap();
        assert_eq!(projection.transcript_handle.kind, "var_handle");
        assert_eq!(projection.transcript_handle.session_id, "worker");
    }

    #[test]
    fn release_transcript_handles_purges_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(HandleStore::new()));
        let mut manager = SubAgentManager::new(
            dir.path().to_path_buf(),
            crate::subagent::types::DEFAULT_MAX_CONCURRENT,
            Arc::clone(&store),
        );
        let boot = manager.session_boot_id.clone();
        manager
            .insert(SubAgentRecord {
                schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
                agent_id: "a1".to_string(),
                name: "worker".to_string(),
                role: "general".to_string(),
                status: SubAgentStatus::Running,
                assignment: "do".to_string(),
                result: None,
                structured: None,
                transcript_handle: None,
                error: None,
                fork_context: false,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                steps_taken: 0,
                session_boot_id: Some(boot),
            })
            .unwrap();
        let record = manager
            .finalize_success("a1", "done".to_string(), 1)
            .unwrap();
        let handle_id = record.transcript_handle.clone().expect("handle");
        assert!(store.read().unwrap().get_summary(&handle_id).is_some());
        manager.release_transcript_handles(&record).unwrap();
        assert!(store.read().unwrap().get_summary(&handle_id).is_none());
    }

    #[test]
    fn reload_marks_running_agents_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(HandleStore::new()));
        {
            let mut manager =
                SubAgentManager::new(dir.path().to_path_buf(), 10, Arc::clone(&store));
            let boot = manager.session_boot_id.clone();
            manager
                .insert(SubAgentRecord {
                    schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
                    agent_id: "a1".to_string(),
                    name: "worker".to_string(),
                    role: "explore".to_string(),
                    status: SubAgentStatus::Running,
                    assignment: "task".to_string(),
                    result: None,
                    structured: None,
                    transcript_handle: None,
                    error: None,
                    fork_context: false,
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    steps_taken: 0,
                    session_boot_id: Some(boot),
                })
                .unwrap();
        }
        let reloaded = SubAgentManager::new(dir.path().to_path_buf(), 10, store);
        let record = reloaded.get("a1").expect("record");
        assert_eq!(record.status, SubAgentStatus::Interrupted);
    }
}
