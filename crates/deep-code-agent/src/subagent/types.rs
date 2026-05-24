use std::fmt;

use serde::{Deserialize, Serialize};

pub const SUBAGENT_STATE_SCHEMA_VERSION: u32 = 1;
pub const SUBAGENT_STATE_FILE: &str = "subagents.v1.json";
pub const DEFAULT_MAX_CONCURRENT: usize = 10;
pub const HARD_MAX_CONCURRENT: usize = 20;
pub const DEFAULT_MAX_STEPS: u32 = 50;
pub const DEFAULT_EVAL_TIMEOUT_MS: u64 = 30_000;
/// Upper bound for blocking waits inside synchronous `agent_eval` tool execution.
pub const MAX_SYNC_EVAL_WAIT_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl SubAgentStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentRecord {
    pub schema_version: u32,
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub status: SubAgentStatus,
    pub assignment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<StructuredReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_handle: Option<crate::handle::HandleId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub fork_context: bool,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub steps_taken: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_boot_id: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentSessionProjection {
    pub name: String,
    pub agent_id: String,
    pub status: String,
    pub terminal: bool,
    pub context_mode: String,
    pub fork_context: bool,
    pub transcript_handle: crate::handle::VarHandle,
    pub snapshot: SubAgentRecord,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentError {
    NotFound { id: String },
    InvalidRole { value: String },
    ConcurrencyLimit { cap: usize },
    InvalidArguments { message: String },
    Io { message: String },
    State { message: String },
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
            Self::Io { message } => write!(formatter, "sub-agent I/O failed: {message}"),
            Self::State { message } => write!(formatter, "sub-agent state error: {message}"),
        }
    }
}

impl std::error::Error for SubAgentError {}
