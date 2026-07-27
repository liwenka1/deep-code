use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session_store::now_ms;
use crate::subagent::output::parse_structured_report;
use crate::subagent::types::{
    HARD_MAX_CONCURRENT, SUBAGENT_STATE_FILE, SUBAGENT_STATE_SCHEMA_VERSION, SubAgentError,
    SubAgentRecord, SubAgentStatus,
};

/// Cap on retained records from *prior* sessions. The current session's agents
/// are never pruned (they are the live working set shown by `/agents`); only
/// older cross-session history is bounded so the ledger file cannot grow
/// without limit over a workspace's lifetime.
const MAX_HISTORY_RECORDS: usize = 100;

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
    state_path: PathBuf,
}

impl SubAgentManager {
    pub fn new(workspace: PathBuf, max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.clamp(1, HARD_MAX_CONCURRENT);
        let state_path = default_state_path(&workspace);
        let mut manager = Self {
            _workspace: workspace,
            max_concurrent,
            session_boot_id: new_boot_id(),
            agents: std::collections::HashMap::new(),
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
        self.prune_history();
        self.persist_state()
    }

    /// Drop the oldest prior-session records beyond [`MAX_HISTORY_RECORDS`],
    /// keeping every current-session record. No-op while history is within cap.
    fn prune_history(&mut self) {
        let current = self.session_boot_id.clone();
        let mut prior: Vec<(String, u64)> = self
            .agents
            .values()
            .filter(|agent| agent.session_boot_id.as_deref() != Some(current.as_str()))
            .map(|agent| (agent.agent_id.clone(), agent.started_at_ms))
            .collect();
        if prior.len() <= MAX_HISTORY_RECORDS {
            return;
        }
        // Newest first, then drop everything past the cap.
        prior.sort_by_key(|(_, started_at)| std::cmp::Reverse(*started_at));
        for (agent_id, _) in prior.into_iter().skip(MAX_HISTORY_RECORDS) {
            self.agents.remove(&agent_id);
        }
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
        // Bound the accumulated cross-session history on every load. All loaded
        // records belong to prior sessions (the current boot id is fresh), so
        // this trims the file back under cap before the new session appends.
        self.prune_history();
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
        write_atomic(&self.state_path, payload.as_bytes())
    }
}

/// Durably write `contents` to `path`: stage to a per-process-unique tmp file,
/// fsync it, rename over the target, then fsync the directory. The unique tmp
/// name (pid + nanos) keeps two deep-code processes sharing one workspace from
/// clobbering each other's staging file; the fsyncs stop a crash from leaving a
/// truncated ledger behind. Mirrors `session_store::write_atomic`.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), SubAgentError> {
    use std::io::Write;

    let io_err = |error: std::io::Error| SubAgentError::Io {
        message: error.to_string(),
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("subagents");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let tmp = parent.join(format!(".{stem}.{}.{nanos}.tmp", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp).map_err(io_err)?;
        file.write_all(contents).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
    }
    std::fs::rename(&tmp, path).map_err(io_err)?;
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_limit_blocks_running_agents() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SubAgentManager::new(dir.path().to_path_buf(), 1);
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
            error: None,
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
        let mut manager = SubAgentManager::new(
            dir.path().to_path_buf(),
            crate::subagent::types::DEFAULT_MAX_CONCURRENT,
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
                error: None,
                started_at_ms: now_ms(),
                finished_at_ms: None,
                steps_taken: 0,
                session_boot_id: Some(boot),
            })
            .unwrap();
        let text = "### SUMMARY\nDone.\n\n### EVIDENCE\nNone.\n\n### CHANGES\nNone.\n\n### RISKS\nNone observed.\n\n### BLOCKERS\nNone.\n";
        let record = manager.finalize_success("a1", text.to_string(), 1).unwrap();
        assert_eq!(record.status, SubAgentStatus::Completed);
        assert!(record.structured.is_some());
        assert_eq!(record.name, "worker");
    }

    #[test]
    fn prune_bounds_prior_session_history_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_state_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 150 terminal records from a prior session, started_at 1..=150.
        let agents: Vec<SubAgentRecord> = (1..=150u64)
            .map(|n| SubAgentRecord {
                schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
                agent_id: format!("a{n}"),
                name: format!("w{n}"),
                role: "general".to_string(),
                status: SubAgentStatus::Completed,
                assignment: "t".to_string(),
                result: None,
                structured: None,
                error: None,
                started_at_ms: n,
                finished_at_ms: Some(n),
                steps_taken: 0,
                session_boot_id: Some("old_boot".to_string()),
            })
            .collect();
        let payload = serde_json::json!({
            "schema_version": SUBAGENT_STATE_SCHEMA_VERSION,
            "session_boot_id": "old_boot",
            "agents": agents,
        });
        std::fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();

        let manager = SubAgentManager::new(dir.path().to_path_buf(), 10);
        // Newest MAX_HISTORY_RECORDS (started 51..=150) kept; oldest 50 dropped.
        assert!(manager.get("a1").is_none(), "oldest must be pruned");
        assert!(manager.get("a50").is_none(), "just past cap must be pruned");
        assert!(manager.get("a51").is_some(), "cap boundary must be kept");
        assert!(manager.get("a150").is_some(), "newest must be kept");
    }

    #[test]
    fn reload_marks_running_agents_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut manager = SubAgentManager::new(dir.path().to_path_buf(), 10);
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
                    error: None,
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    steps_taken: 0,
                    session_boot_id: Some(boot),
                })
                .unwrap();
        }
        let reloaded = SubAgentManager::new(dir.path().to_path_buf(), 10);
        let record = reloaded.get("a1").expect("record");
        assert_eq!(record.status, SubAgentStatus::Interrupted);
    }
}
