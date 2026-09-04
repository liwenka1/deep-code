use std::fmt;

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_CONCURRENT: usize = 10;
pub const HARD_MAX_CONCURRENT: usize = 20;
pub const DEFAULT_MAX_STEPS: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SubAgentStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredReport {
    pub summary: String,
    pub evidence: String,
    pub changes: String,
    pub risks: String,
    pub blockers: String,
}

/// One sub-agent's live/terminal state, held in memory for the session (see
/// [`super::manager::SubAgentManager`]). Not persisted — the durable copy of a
/// sub-agent's work is its report in the parent transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentRecord {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub status: SubAgentStatus,
    pub assignment: String,
    pub result: Option<String>,
    pub structured: Option<StructuredReport>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub steps_taken: u32,
}

impl SubAgentRecord {
    #[must_use]
    pub fn short_summary(&self) -> String {
        if let Some(structured) = &self.structured {
            return structured.summary.clone();
        }
        self.result
            .as_deref()
            .map(|text| {
                let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if flattened.chars().count() > 120 {
                    format!("{}...", flattened.chars().take(120).collect::<String>())
                } else {
                    flattened
                }
            })
            .unwrap_or_else(|| self.status.as_str().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentError {
    NotFound { id: String },
    InvalidRole { value: String },
    ConcurrencyLimit { cap: usize },
    InvalidArguments { message: String },
}

impl fmt::Display for SubAgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(formatter, "sub-agent '{id}' was not found"),
            Self::InvalidRole { value } => {
                write!(
                    formatter,
                    "unknown sub-agent role '{value}' (accepted: general, explore, plan, review, implementer, verifier)"
                )
            }
            Self::ConcurrencyLimit { cap } => {
                write!(formatter, "sub-agent concurrency limit reached ({cap})")
            }
            Self::InvalidArguments { message } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for SubAgentError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_str` hand-spells what `#[serde(rename_all = "snake_case")]`
    /// derives; pin the two together so the duplication cannot drift.
    #[test]
    fn subagent_status_as_str_is_its_serde_spelling() {
        for status in [
            SubAgentStatus::Running,
            SubAgentStatus::Completed,
            SubAgentStatus::Failed,
            SubAgentStatus::Cancelled,
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().to_string()),
                "{status:?}"
            );
        }
    }
}
