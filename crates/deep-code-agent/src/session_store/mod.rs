//! Persistent session storage for long-running agent conversations.
//!
//! The default backend is JSON files under `.deep-code/sessions/`. The
//! [`SessionStore`] trait is intentionally narrow so a SQLite backend can
//! replace JSON later without touching the runtime.

mod json;
mod migrate;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use json::JsonSessionStore;

use crate::checkpoint::CheckpointId;
use crate::config::AgentConfig;
use crate::model::Usage;
use crate::session_entry::{EntryKind, SessionEntry};
use crate::tool::ToolResult;

/// Current on-disk schema version. Bump when making breaking layout changes.
/// v1 stored wire messages; v2 stores [`SessionEntry`] values (v1 files are
/// migrated transparently on load).
pub const SESSION_SCHEMA_VERSION: u32 = 2;

const SESSIONS_DIR: &str = ".deep-code/sessions";

/// Opaque session identifier (filename stem under the sessions directory).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse and validate a user-supplied session id.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, SessionStoreError> {
        let value = value.as_ref();
        validate_session_id(value)?;
        Ok(Self(value.to_string()))
    }
}

/// Serializable agent config snapshot (API key is never stored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub base_url: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub auto_model: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
    pub timeout_secs: Option<u64>,
    pub api_key_present: bool,
}

impl From<&AgentConfig> for ConfigSnapshot {
    fn from(config: &AgentConfig) -> Self {
        Self {
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            reasoning_effort: Some(config.reasoning_effort.as_setting().to_string()),
            auto_model: config.auto_model_enabled(),
            cost_currency: Some(format!("{:?}", config.cost_currency).to_ascii_lowercase()),
            timeout_secs: config.timeout.map(|duration| duration.as_secs()),
            api_key_present: config
                .api_key
                .as_ref()
                .is_some_and(|key| !key.trim().is_empty()),
        }
    }
}

/// One user turn and its tool activity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub user_prompt: String,
    pub tool_results: Vec<ToolResult>,
    pub usage: Option<Usage>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// Checkpoint metadata retained in the session for UI/API resume projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: CheckpointId,
    pub label: String,
    pub created_at_ms: u64,
}

impl CheckpointRecord {
    #[must_use]
    pub fn new(id: CheckpointId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            created_at_ms: now_ms(),
        }
    }
}

impl TurnRecord {
    #[must_use]
    pub fn new(user_prompt: impl Into<String>) -> Self {
        Self {
            user_prompt: user_prompt.into(),
            tool_results: Vec::new(),
            usage: None,
            started_at_ms: now_ms(),
            finished_at_ms: None,
        }
    }

    pub fn finish(&mut self, usage: Option<Usage>) {
        self.usage = usage;
        self.finished_at_ms = Some(now_ms());
    }
}

/// Full persisted session state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: SessionId,
    pub workspace: PathBuf,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub config: ConfigSnapshot,
    /// Domain-level conversation entries (schema v2). Wire messages are
    /// derived via [`crate::session::Session::wire_messages`].
    pub entries: Vec<SessionEntry>,
    pub turns: Vec<TurnRecord>,
    /// Workspace snapshots created during this session.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointRecord>,
    /// Transcript summary produced by compaction. Derived field mirroring the
    /// latest Compaction entry, kept for status lines and events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Compaction metadata, e.g. `archived=N` (when applied). Derived field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
}

impl SessionRecord {
    #[must_use]
    pub fn new(workspace: PathBuf, config: &AgentConfig, system_prompt: impl Into<String>) -> Self {
        let now = now_ms();
        let entries = vec![SessionEntry::system(system_prompt)];
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: new_session_id(),
            workspace,
            created_at_ms: now,
            updated_at_ms: now,
            config: ConfigSnapshot::from(config),
            entries,
            turns: Vec::new(),
            checkpoints: Vec::new(),
            summary: None,
            compaction: None,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at_ms = now_ms();
    }

    pub fn preview(&self) -> String {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::User { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "(empty session)".to_string())
    }

    #[must_use]
    pub fn has_user_entry(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::User { .. }))
    }

    /// Derived wire-message count. Consumers report this (not the entry
    /// count) so displayed numbers stay stable across the v1→v2 migration.
    #[must_use]
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .map(SessionEntry::wire_message_count)
            .sum()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("session '{id}' was not found")]
    NotFound { id: String },
    #[error("session storage I/O failed: {message}")]
    Io { message: String },
    #[error("session storage serialization failed: {message}")]
    Serialization { message: String },
    #[error("unsupported session schema version {found} (expected {expected})")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("invalid session id '{id}'")]
    InvalidId { id: String },
}

/// Backend-agnostic session persistence.
pub trait SessionStore: Send + Sync {
    fn save(&self, record: &SessionRecord) -> Result<(), SessionStoreError>;
    fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError>;
    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError>;
    fn delete(&self, id: &SessionId) -> Result<(), SessionStoreError>;
    fn export(&self, id: &SessionId) -> Result<String, SessionStoreError> {
        let record = self.load(id)?;
        serde_json::to_string_pretty(&record).map_err(|error| SessionStoreError::Serialization {
            message: error.to_string(),
        })
    }
}

#[must_use]
pub fn new_session_id() -> SessionId {
    static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    SessionId(format!("session_{}_{seq}", now_ms()))
}

/// Reject path components and other unsafe filename characters in session ids.
pub fn validate_session_id(id: &str) -> Result<(), SessionStoreError> {
    if id.is_empty() || id.len() > 128 {
        return Err(SessionStoreError::InvalidId { id: id.to_string() });
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(SessionStoreError::InvalidId { id: id.to_string() });
    }
    Ok(())
}

#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[must_use]
pub fn sessions_dir_for_workspace(workspace: &Path) -> PathBuf {
    workspace.join(SESSIONS_DIR)
}

/// Human-readable note that sessions are scoped to a workspace directory.
#[must_use]
pub fn format_sessions_storage_note(workspace: &Path) -> String {
    format!(
        "Sessions are stored under {} (one pool per workspace directory; run deep-code from the same cwd to list or resume).",
        sessions_dir_for_workspace(workspace).display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_id_rejects_traversal() {
        assert!(matches!(
            validate_session_id("../evil"),
            Err(SessionStoreError::InvalidId { .. })
        ));
        assert!(validate_session_id("session_123_0").is_ok());
    }

    #[test]
    fn session_id_parse_validates_input() {
        assert!(SessionId::parse("session_ok-1").is_ok());
        assert!(SessionId::parse("../../tmp/x").is_err());
    }

    #[test]
    fn session_record_preview_uses_latest_user_entry() {
        let mut record =
            SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "system");
        record.entries.push(SessionEntry::user("first"));
        record
            .entries
            .push(SessionEntry::assistant("ok", None, Vec::new()));
        record.entries.push(SessionEntry::user("second"));

        assert_eq!(record.preview(), "second");
        assert!(record.has_user_entry());
        assert_eq!(record.message_count(), 4);
    }

    #[test]
    fn config_snapshot_omits_api_key() {
        let config = AgentConfig {
            api_key: Some("secret".to_string()),
            ..AgentConfig::default()
        };
        let snapshot = ConfigSnapshot::from(&config);
        assert!(snapshot.api_key_present);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("secret"));
    }

    #[test]
    fn turn_record_tracks_tool_results() {
        let mut turn = TurnRecord::new("hello");
        turn.tool_results.push(ToolResult::success(
            "call_1",
            "mock_echo",
            "mock_echo: hello",
        ));
        turn.finish(None);
        assert!(turn.finished_at_ms.is_some());
        assert_eq!(turn.tool_results.len(), 1);
    }
}
