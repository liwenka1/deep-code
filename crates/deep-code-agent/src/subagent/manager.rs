use crate::session_store::now_ms;
use crate::subagent::output::parse_structured_report;
use crate::subagent::types::{HARD_MAX_CONCURRENT, SubAgentError, SubAgentRecord, SubAgentStatus};

/// In-memory ledger of the current session's sub-agents.
///
/// Blocking sub-agents return their report straight into the parent
/// transcript (the durable record), so this holds only live working-set state:
/// what `/agents` lists and the concurrency cap. It is never persisted and
/// starts empty each session — a fresh manager is built per runtime launch, so
/// every record it holds belongs to the current session by construction.
pub struct SubAgentManager {
    max_concurrent: usize,
    agents: std::collections::HashMap<String, SubAgentRecord>,
}

impl SubAgentManager {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.clamp(1, HARD_MAX_CONCURRENT),
            agents: std::collections::HashMap::new(),
        }
    }

    /// Test-only: shrink the concurrency cap after construction so the
    /// registry-path race test can force cap=1 (the production attach path
    /// hardcodes `DEFAULT_MAX_CONCURRENT`). No runtime reconfigures concurrency.
    #[cfg(test)]
    pub fn set_max_concurrent(&mut self, max_concurrent: usize) {
        self.max_concurrent = max_concurrent.clamp(1, HARD_MAX_CONCURRENT);
    }

    pub fn running_count(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| agent.status == SubAgentStatus::Running)
            .count()
    }

    pub fn list_current_session(&self) -> Vec<SubAgentRecord> {
        let mut agents: Vec<_> = self.agents.values().cloned().collect();
        agents.sort_by_key(|agent| std::cmp::Reverse(agent.started_at_ms));
        agents
    }

    pub fn get(&self, agent_id: &str) -> Option<&SubAgentRecord> {
        self.agents.get(agent_id)
    }

    pub fn insert(&mut self, record: SubAgentRecord) -> Result<(), SubAgentError> {
        if record.status == SubAgentStatus::Running && self.running_count() >= self.max_concurrent {
            return Err(SubAgentError::ConcurrencyLimit {
                cap: self.max_concurrent,
            });
        }
        self.agents.insert(record.agent_id.clone(), record);
        Ok(())
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
        Ok(())
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
            if record.status == SubAgentStatus::Running {
                record.status = SubAgentStatus::Cancelled;
                record.finished_at_ms = Some(now_ms());
                record.error = Some("parent session cancelled".to_string());
            }
        }
    }
}

pub fn new_agent_id() -> String {
    format!("agent_{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::types::DEFAULT_MAX_CONCURRENT;

    fn record(agent_id: &str, name: &str, status: SubAgentStatus) -> SubAgentRecord {
        SubAgentRecord {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
            role: "explore".to_string(),
            status,
            assignment: "task".to_string(),
            result: None,
            structured: None,
            error: None,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            steps_taken: 0,
        }
    }

    #[test]
    fn concurrency_limit_blocks_running_agents() {
        let mut manager = SubAgentManager::new(1);
        manager
            .insert(record("a1", "first", SubAgentStatus::Running))
            .unwrap();
        assert!(matches!(
            manager.insert(record("a2", "second", SubAgentStatus::Running)),
            Err(SubAgentError::ConcurrencyLimit { .. })
        ));
    }

    #[test]
    fn finalize_success_parses_structured_report() {
        let mut manager = SubAgentManager::new(DEFAULT_MAX_CONCURRENT);
        manager
            .insert(record("a1", "worker", SubAgentStatus::Running))
            .unwrap();
        let text = "### SUMMARY\nDone.\n\n### EVIDENCE\nNone.\n\n### CHANGES\nNone.\n\n### RISKS\nNone observed.\n\n### BLOCKERS\nNone.\n";
        let record = manager.finalize_success("a1", text.to_string(), 1).unwrap();
        assert_eq!(record.status, SubAgentStatus::Completed);
        assert!(record.structured.is_some());
        assert_eq!(record.name, "worker");
    }
}
