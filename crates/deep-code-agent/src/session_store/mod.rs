//! Persistent session storage for long-running agent conversations.
//!
//! The default backend is JSON files under `.deep-code/sessions/`. The
//! [`SessionStore`] trait is intentionally narrow so a SQLite backend can
//! replace JSON later without touching the runtime.

mod json;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use json::JsonSessionStore;

/// Make sure `<state_dir>/.gitignore` excludes everything, for every writer of
/// `<workspace>/.deep-code` — not just whichever one happens to run first.
pub(crate) const SELF_IGNORE_BODY: &str = "# Written by deep-code: this directory holds session transcripts and logs.\n\
             # Deleting this file does not opt out — deep-code writes it back next session.\n\
             # To commit this directory after all, edit this file instead (e.g. remove the\n\
             # `*` line); deep-code never touches an existing .gitignore.\n\
             *\n";

use crate::checkpoint::CheckpointId;
use crate::pricing::CostEstimate;
use crate::session_entry::{EntryKind, SessionEntry};

/// Current on-disk schema version. Bump when making breaking layout changes.
/// Schema v3 is the authenticated record format. Older layouts are outside the
/// current storage contract.
pub const SESSION_SCHEMA_VERSION: u32 = 3;

/// Hard limits for the JSON backend. Session files live in the workspace and
/// can therefore be written by the model; loading them must not turn into an
/// unbounded allocation or a picker-wide memory blow-up.
pub(crate) const MAX_SESSION_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_SESSION_ENTRIES: usize = 4096;
/// Maximum number of records retained by the picker/list API.
pub(crate) const MAX_SESSION_LIST: usize = 32;

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
/// are ever read (the turn count and the checkpoint-to-turn time window).
/// `SessionRecord` has no `deny_unknown_fields`, so harmless extra keys remain
/// ignorable within the current schema.
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
/// MUST NOT gain `#[serde(deny_unknown_fields)]` — an unknown key is ignored so
/// a record written by a newer build stays readable, and whether it can be
/// trusted is the host tag's decision, not the parser's.
///
/// **Adding or removing a persisted field REQUIRES bumping
/// `SESSION_SCHEMA_VERSION`.** `record_mac` signs a re-serialization of the
/// parsed struct (see `session_integrity`), so the shape of the struct is part
/// of the signed bytes: a new field changes the message for every existing
/// record, verification then fails, and the transcript is discarded on the next
/// resume. The version check turns that into an explicit "unsupported schema"
/// rejection instead. The same holds for [`TurnRecord`], [`SessionEntry`],
/// [`EntryKind`], and every other nested type reachable from here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: SessionId,
    pub workspace: PathBuf,
    /// Extra writable roots granted at launch (`--add-dir`), canonical. Kept in
    /// the record so `-c`/`--resume` restores the same boundary the session was
    /// working under — a resume that silently dropped a grant would break the
    /// model mid-task on paths it legitimately wrote last turn. Defaulted when
    /// the current record carries no grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_roots: Vec<PathBuf>,
    /// Authorship tag over [`Self::extra_roots`], stamped on every save (see
    /// the `session_integrity` module). The record is a file the model can write
    /// and on resume its grants become the write boundary, so the grants must
    /// prove they came from this host rather than merely appearing in the
    /// file. A list that does not verify is dropped on resume. Absent — and
    /// unnecessary — when nothing is granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_roots_mac: Option<String>,
    /// Authorship tag over the complete persisted record. The root tag above
    /// remains useful for the narrow grant check, while this tag prevents a
    /// workspace-writable session file from injecting transcript entries,
    /// timestamps, or other persisted state into a resumed runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_mac: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Domain-level conversation entries (schema v3). Wire messages are
    /// derived via [`crate::session::Session::wire_messages`]. Shared by
    /// `Arc` with the live [`crate::session::Session`], so flushing the
    /// transcript into the record copies pointers, not entry bytes; serde
    /// serializes through the `Arc` transparently (same on-disk JSON).
    pub entries: Vec<std::sync::Arc<SessionEntry>>,
    pub turns: Vec<TurnRecord>,
    /// Workspace snapshots created during this session.
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
    /// flushed here on every save and restored on resume.
    pub session_cost: CostEstimate,
    pub session_cache_hit_tokens: u64,
    pub session_cache_miss_tokens: u64,
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
            record_mac: None,
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
    /// Older on-disk formats are rejected before this constructor is used.
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
                EntryKind::User { content, .. } => Some(content.clone()),
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
    /// count) so displayed numbers stay stable across persisted sessions.
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
    #[error("session scan cancelled")]
    Cancelled,
    #[error("session '{id}' was not found")]
    NotFound { id: String },
    #[error("session storage I/O failed: {message}")]
    Io { message: String },
    #[error("session storage serialization failed: {message}")]
    Serialization { message: String },
    #[error("session '{id}' could not be authenticated for persistence")]
    AuthenticationUnavailable { id: String },
    #[error("no session signing key is available: {detail}")]
    KeyUnavailable { detail: String },
    #[error("session file {path} is too large ({bytes} bytes; limit {limit})")]
    TooLarge {
        path: String,
        bytes: u64,
        limit: u64,
    },
    #[error("session file {path} has too many entries ({count}; limit {limit})")]
    TooManyEntries {
        path: String,
        count: usize,
        limit: usize,
    },
    #[error("unsupported session schema version {found} (expected {expected})")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("invalid session id '{id}'")]
    InvalidId { id: String },
}

/// Serialize a record to the on-disk JSON form. Shared by the default
/// [`SessionStore::save`] and the persistence actor, which serializes under the
/// record lock and writes with it released — never deep-cloning the record just
/// to snapshot it.
///
/// Sealing belongs to saving, which is why this (and [`SessionStore::save`])
/// take `&mut`: both tags have to describe the record as it is being written,
/// and every persistence path funnels through here, so there is no way to write
/// a record whose transcript or grants are unsigned or stale.
///
/// The entry-count and size limits are NOT re-checked here: they belong to
/// [`save_serialized`](SessionStore::save_serialized), which knows the
/// destination path and can name it in the error. Repeating them here filled
/// the error's `path` field with a session id.
pub(crate) fn serialize_record(record: &mut SessionRecord) -> Result<String, SessionStoreError> {
    crate::session_integrity::ensure_key().map_err(|reason| SessionStoreError::KeyUnavailable {
        detail: reason.to_string(),
    })?;
    record.extra_roots_mac = crate::session_integrity::sign_roots(
        record.id.as_str(),
        &record.workspace,
        &record.extra_roots,
    );
    record.record_mac = Some(
        crate::session_integrity::sign_record(record).ok_or_else(|| {
            SessionStoreError::Serialization {
                message: "the session record could not be serialized for signing".to_string(),
            }
        })?,
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
    // The `_seq` counter is per-process and starts at 0, so two separate
    // processes creating a session in the same millisecond (parallel `deepcode`
    // / scripted `-p` in one workspace) both minted `session_<ms>_0` and
    // clobbered each other's file via the atomic rename. The pid disambiguates
    // cross-process; the atomic seq disambiguates within a process. Both stay
    // inside the `[A-Za-z0-9_-]` id charset `validate_session_id` enforces.
    SessionId(format!("session_{}_{}_{seq}", now_ms(), std::process::id()))
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
/// Where this workspace's sessions live, and how to get back to them.
///
/// Takes the invoked program name rather than spelling one. `deep-code` was
/// hardcoded here while every other command-line line resolved the name
/// through `cli::program_name` — so this one told npm users (whose command is
/// `deepcode`) to run something that is not on their PATH.
pub fn format_sessions_storage_note(workspace: &Path, program: &str) -> String {
    format!(
        "Sessions are stored under {} (one pool per workspace directory; run {program} from the same cwd to list or resume).",
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

        // Defensive branch: a record with no system entry gets one inserted at
        // the front rather than silently keeping none.
        record.entries.remove(0);
        record.set_system_prompt("fresh prompt");
        assert!(matches!(
            &record.entries[0].kind,
            EntryKind::System { content } if content == "fresh prompt"
        ));
        assert_eq!(record.entries.len(), 2);
    }
}
