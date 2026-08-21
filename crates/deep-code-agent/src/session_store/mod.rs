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
use crate::pricing::CostEstimate;
use crate::session_entry::{EntryKind, SessionEntry};

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

/// One user turn's boundary timestamp. Only `started_at_ms` and `turns.len()`
/// are ever read (the turn count and the checkpoint-to-turn time window); the
/// prompt/usage/finish-time this once carried were write-only, so they were
/// dropped. Old session files that still contain them load fine — `SessionRecord`
/// has no `deny_unknown_fields`, so serde ignores the extra keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub started_at_ms: u64,
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
    pub fn new() -> Self {
        Self {
            started_at_ms: now_ms(),
        }
    }
}

impl Default for TurnRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// Full persisted session state.
///
/// MUST NOT gain `#[serde(deny_unknown_fields)]` — loading old session files
/// depends on unknown keys being ignored. Fields removed over time (e.g. the
/// former `config` snapshot and per-turn `tool_results`) still appear in files
/// written by older builds; deny-unknown would make every such file
/// unloadable. The same holds for [`TurnRecord`], [`SessionEntry`], and
/// [`EntryKind`]. New fields are added with `#[serde(default)]` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: SessionId,
    pub workspace: PathBuf,
    /// Extra writable roots granted at launch (`--add-dir`), canonical. Kept in
    /// the record so `-c`/`--resume` restores the same boundary the session was
    /// working under — a resume that silently dropped a grant would break the
    /// model mid-task on paths it legitimately wrote last turn. Defaulted for
    /// files that predate the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_roots: Vec<PathBuf>,
    /// Authorship tag over [`Self::extra_roots`], stamped on every save (see
    /// [`crate::session_integrity`]). The record is a file the model can write
    /// and on resume its grants become the write boundary, so the grants must
    /// prove they came from this host rather than merely appearing in the
    /// file. A list that does not verify is dropped on resume. Absent — and
    /// unnecessary — when nothing is granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_roots_mac: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Domain-level conversation entries (schema v2). Wire messages are
    /// derived via [`crate::session::Session::wire_messages`]. Shared by
    /// `Arc` with the live [`crate::session::Session`], so flushing the
    /// transcript into the record copies pointers, not entry bytes; serde
    /// serializes through the `Arc` transparently (same on-disk JSON).
    pub entries: Vec<std::sync::Arc<SessionEntry>>,
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
    /// Cumulative token cost over the session's whole lifetime. Persisted so a
    /// resumed session's displayed total continues from where it left off
    /// instead of resetting to zero. The runtime's in-memory accumulators are
    /// flushed here on every save and restored on resume. Defaulted for old
    /// files that predate the field (they resume at zero, as before).
    #[serde(default)]
    pub session_cost: CostEstimate,
    #[serde(default)]
    pub session_cache_hit_tokens: u64,
    #[serde(default)]
    pub session_cache_miss_tokens: u64,
    #[serde(default)]
    pub session_cache_savings: CostEstimate,
}

impl SessionRecord {
    #[must_use]
    pub fn new(workspace: PathBuf, system_prompt: impl Into<String>) -> Self {
        let now = now_ms();
        let entries = vec![std::sync::Arc::new(SessionEntry::system(system_prompt))];
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: new_session_id(),
            workspace,
            extra_roots: Vec::new(),
            extra_roots_mac: None,
            created_at_ms: now,
            updated_at_ms: now,
            entries,
            turns: Vec::new(),
            checkpoints: Vec::new(),
            summary: None,
            compaction: None,
            session_cost: CostEstimate::default(),
            session_cache_hit_tokens: 0,
            session_cache_miss_tokens: 0,
            session_cache_savings: CostEstimate::default(),
        }
    }

    /// Record the launch-granted extra writable roots. Builder-style so the
    /// many existing single-root `new` callers stay untouched.
    #[must_use]
    pub fn with_extra_roots(mut self, extra_roots: Vec<PathBuf>) -> Self {
        self.extra_roots = extra_roots;
        self
    }

    /// Replace the stored system prompt in place. The prompt is `entries[0]`
    /// by construction; resume rebuilds it so the model sees the *current*
    /// granted roots — the saved prompt only ever names the grants that
    /// existed when the session was created, and a root added on `-c
    /// --add-dir` (or dropped as stale) would otherwise stay invisible for
    /// the rest of the session's life. Defensive about position anyway:
    /// v1-migrated files are shaped by old data, not this constructor.
    pub fn set_system_prompt(&mut self, system_prompt: impl Into<String>) {
        let entry = std::sync::Arc::new(SessionEntry::system(system_prompt.into()));
        let slot = self
            .entries
            .iter()
            .position(|existing| matches!(existing.kind, EntryKind::System { .. }));
        match slot {
            Some(index) => self.entries[index] = entry,
            None => self.entries.insert(0, entry),
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
            .map(|entry| entry.wire_message_count())
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

/// Serialize a record to the on-disk JSON form. Shared by the default
/// [`SessionStore::save`] and the persistence actor, which serializes under the
/// record lock and writes with it released — avoiding a full-record clone.
/// Seal the record's grant list, then serialize.
///
/// Sealing belongs to saving, which is why this (and [`SessionStore::save`])
/// take `&mut`: the tag has to describe the grants as they are being written,
/// and every persistence path funnels through here, so there is no way to
/// write a record whose grants are unsigned or stale.
pub(crate) fn serialize_record(record: &mut SessionRecord) -> Result<String, SessionStoreError> {
    record.extra_roots_mac = crate::session_integrity::sign_roots(
        record.id.as_str(),
        &record.workspace,
        &record.extra_roots,
    );
    serde_json::to_string_pretty(record).map_err(|error| SessionStoreError::Serialization {
        message: error.to_string(),
    })
}

/// Backend-agnostic session persistence.
pub trait SessionStore: Send + Sync {
    /// Serialize then persist a record. Provided in terms of
    /// [`save_serialized`](SessionStore::save_serialized); the persistence actor
    /// calls that directly so it can serialize under the record mutex and write
    /// with the lock released, never deep-cloning the record just to snapshot it.
    fn save(&self, record: &mut SessionRecord) -> Result<(), SessionStoreError> {
        self.save_serialized(&record.id.clone(), &serialize_record(record)?)
    }
    /// Write an already-serialized record body under `id`.
    fn save_serialized(&self, id: &SessionId, json: &str) -> Result<(), SessionStoreError>;
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
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "system");
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user("first")));
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::assistant(
                "ok",
                None,
                Vec::new(),
            )));
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user("second")));

        assert_eq!(record.preview(), "second");
        assert!(record.has_user_entry());
        assert_eq!(record.message_count(), 4);
    }

    #[test]
    fn set_system_prompt_replaces_in_place_and_survives_a_missing_slot() {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "old prompt");
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user("hi")));
        record.set_system_prompt("new prompt");
        // Replaced where it sat — position and the rest of the history intact.
        assert!(matches!(
            &record.entries[0].kind,
            EntryKind::System { content } if content == "new prompt"
        ));
        assert_eq!(record.entries.len(), 2);

        // Defensive branch: a (v1-shaped) record with no system entry gets one
        // inserted at the front rather than silently keeping none.
        record.entries.remove(0);
        record.set_system_prompt("fresh prompt");
        assert!(matches!(
            &record.entries[0].kind,
            EntryKind::System { content } if content == "fresh prompt"
        ));
        assert_eq!(record.entries.len(), 2);
    }

    /// v2 files written before `config`, per-turn `tool_results`, and entry
    /// `id`/`parent` were dropped still carry those keys; loading must ignore
    /// them instead of failing.
    #[test]
    fn v2_files_with_since_removed_fields_still_parse() {
        let legacy = serde_json::json!({
            "schema_version": 2,
            "id": "session_1_0",
            "workspace": "/tmp/ws",
            "created_at_ms": 1,
            "updated_at_ms": 2,
            "config": {"base_url": "x", "model": "m", "timeout_secs": null, "api_key_present": false},
            "entries": [
                {"id": "e1_0", "parent": null, "type": "system", "content": "sys"},
                {"id": "e2_1", "parent": "e1_0", "type": "user", "content": "hi"}
            ],
            "turns": [{
                "user_prompt": "hi",
                "tool_results": [{"call_id": "c1", "tool_name": "shell", "content": "ok", "status": "success"}],
                "usage": null,
                "started_at_ms": 1,
                "finished_at_ms": 2
            }],
        });
        let record: SessionRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(record.entries.len(), 2);
        assert_eq!(record.preview(), "hi");
        assert_eq!(record.turns.len(), 1);
        // The slimmed TurnRecord keeps only started_at_ms; the old
        // user_prompt/usage/finished_at_ms keys are ignored, not rejected.
        assert_eq!(record.turns[0].started_at_ms, 1);
    }
}
