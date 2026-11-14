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
use crate::runtime::TurnId;
use crate::session_entry::{EntryKind, SessionEntry};

/// Hard limits for the JSON backend. Session files live in the workspace and
/// can therefore be written by the model; loading them must not turn into an
/// unbounded allocation or a picker-wide memory blow-up.
pub(crate) const MAX_SESSION_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_SESSION_ENTRIES: usize = 4096;
/// Image-report history has its own ceiling, independent of model context.
pub(crate) const IMAGE_REPORT_BUDGET_BYTES: usize = 512 * 1024;
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

/// One runtime turn, including any user steering injected while it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub id: TurnId,
    pub started_at_ms: u64,
    /// Changes in image projection, not image bytes or HTTP-success claims.
    pub image_requests: Vec<ImageRequestRecord>,
    /// Cumulative observation reports removed by the retention budget.
    pub image_reports_omitted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRequestRecord {
    pub model: String,
    pub images: Vec<crate::image::ImageDelivery>,
}

/// Latest projection fingerprint for an observation still in the transcript.
/// Independent of report history so pruning reports does not reset deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageProjectionState {
    pub observation_id: String,
    pub fingerprint: String,
}

/// Checkpoint metadata retained in the session for UI/API resume projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: CheckpointId,
    pub turn_id: TurnId,
    pub label: String,
    pub created_at_ms: u64,
}

impl CheckpointRecord {
    #[must_use]
    pub fn new(id: CheckpointId, turn_id: TurnId, label: impl Into<String>) -> Self {
        Self {
            id,
            turn_id,
            label: label.into(),
            created_at_ms: now_ms(),
        }
    }
}

impl TurnRecord {
    #[must_use]
    pub fn new(id: TurnId) -> Self {
        Self {
            id,
            started_at_ms: now_ms(),
            image_requests: Vec::new(),
            image_reports_omitted: 0,
        }
    }

    pub(crate) fn limit_image_reports(&mut self, remaining: &mut usize) {
        for request in self.image_requests.iter_mut().rev() {
            // Include the model's JSON escaping and ample request/array framing.
            let overhead = report_storage_cost(&request.model).saturating_add(512);
            let mut first_kept = request.images.len();
            if let Some(mut room) = remaining.checked_sub(overhead) {
                for image in request.images.iter().rev() {
                    let cost = report_storage_cost(image).saturating_add(32);
                    let Some(next) = room.checked_sub(cost) else {
                        break;
                    };
                    room = next;
                    first_kept -= 1;
                }
                if first_kept < request.images.len() {
                    *remaining = room;
                }
            }
            self.image_reports_omitted =
                self.image_reports_omitted.saturating_add(first_kept as u64);
            request.images.drain(..first_kept);
        }
        self.image_requests
            .retain(|request| !request.images.is_empty());
    }
}

fn report_storage_cost(value: &impl Serialize) -> usize {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len()).saturating_add(
                bytes
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count()
                    .saturating_mul(32),
            );
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    serde_json::to_writer_pretty(&mut counter, value).expect("image report data serializes");
    counter.0
}

impl Default for TurnRecord {
    fn default() -> Self {
        Self::new(TurnId::new())
    }
}

/// Current on-disk contract. Unsupported versions are rejected, never migrated.
const SESSION_FORMAT_VERSION: u32 = 1;

fn deserialize_format_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version != SESSION_FORMAT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "unsupported session format {version}; expected {SESSION_FORMAT_VERSION}"
        )));
    }
    Ok(version)
}

/// Full persisted session state.
///
/// The current structure is the accepted payload. Unknown or missing required
/// fields are errors; trust is established by the host tag over the parsed record.
///
/// `record_mac` signs a re-serialization of this struct (see
/// `session_integrity`), so the persisted field set defines the signed bytes.
/// The loader parses this structure directly and authenticates it before any
/// persisted state is exposed to callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    #[serde(deserialize_with = "deserialize_format_version")]
    pub format_version: u32,
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
    /// Domain-level conversation entries. Wire messages are
    /// derived via [`crate::session::Session::wire_messages`]. Shared by
    /// `Arc` with the live [`crate::session::Session`], so flushing the
    /// transcript into the record copies pointers, not entry bytes; serde
    /// serializes through the `Arc` transparently (same on-disk JSON).
    pub entries: Vec<std::sync::Arc<SessionEntry>>,
    pub turns: Vec<TurnRecord>,
    pub image_projection_state: Vec<ImageProjectionState>,
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
    pub(crate) fn limit_image_reports(&mut self) {
        let mut remaining = IMAGE_REPORT_BUDGET_BYTES;
        for turn in self.turns.iter_mut().rev() {
            turn.limit_image_reports(&mut remaining);
        }
    }
    /// Leave room for the next model/tool batch before the JSON backend's hard
    /// limits. Short messages and JSON escaping can exhaust disk limits long
    /// before the token-based context threshold is reached.
    pub(crate) fn approaching_storage_limit(&self) -> bool {
        if self.entries.len() >= MAX_SESSION_ENTRIES * 3 / 4 {
            return true;
        }
        struct Budget(u64);
        impl std::io::Write for Budget {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.checked_sub(bytes.len() as u64).ok_or_else(|| {
                    std::io::Error::other("session compaction size threshold reached")
                })?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Count the actual on-disk representation, including metadata, without
        // allocating a second copy of the transcript. An unserializable record
        // also needs attention rather than being treated as safely small.
        serde_json::to_writer_pretty(Budget(MAX_SESSION_FILE_BYTES * 3 / 4), self).is_err()
    }

    #[must_use]
    pub fn new(workspace: PathBuf, system_prompt: impl Into<String>) -> Self {
        let now = now_ms();
        let entries = vec![std::sync::Arc::new(SessionEntry::system(system_prompt))];
        Self {
            format_version: SESSION_FORMAT_VERSION,
            id: new_session_id(),
            workspace,
            extra_roots: Vec::new(),
            extra_roots_mac: None,
            record_mac: None,
            created_at_ms: now,
            updated_at_ms: now,
            entries,
            turns: Vec::new(),
            image_projection_state: Vec::new(),
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
    /// the rest of the session's life. A missing system entry is inserted.
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
                EntryKind::User { content, .. } => Some(content.summary_text()),
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
    #[error("session file {path} failed integrity verification")]
    AuthenticationFailed { path: String },
    #[error("invalid session id '{id}'")]
    InvalidId { id: String },
}

/// A host-sealed snapshot for a persistence backend. Its private fields bind
/// the destination and validation metadata to the encoded body: callers cannot
/// hand a backend arbitrary JSON or a mismatching session id.
pub struct PreparedSession {
    id: SessionId,
    json: String,
    entry_count: usize,
}

impl PreparedSession {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn json(&self) -> &str {
        &self.json
    }
}

/// Seal and encode under the record lock; the resulting owned body can be
/// written after releasing the lock without decoding or authenticating it again.
pub(crate) fn prepare_record(
    record: &mut SessionRecord,
) -> Result<PreparedSession, SessionStoreError> {
    record.limit_image_reports();
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
    let json = serde_json::to_string_pretty(&record).map_err(|error| {
        SessionStoreError::Serialization {
            message: error.to_string(),
        }
    })?;
    Ok(PreparedSession {
        id: record.id.clone(),
        json,
        entry_count: record.entries.len(),
    })
}

/// Backend-agnostic session persistence.
pub trait SessionStore: Send + Sync {
    /// Seal, encode, and persist a record.
    fn save(&self, record: &mut SessionRecord) -> Result<(), SessionStoreError> {
        self.save_prepared(&prepare_record(record)?)
    }
    /// Write an encoded snapshot produced by the host's signing path.
    fn save_prepared(&self, session: &PreparedSession) -> Result<(), SessionStoreError>;
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
    fn image_report_retention_budgets_serialized_bytes_and_is_idempotent() {
        let mut record = SessionRecord::new(PathBuf::from("/workspace"), "system");
        let mut turn = TurnRecord::default();
        turn.image_requests.push(ImageRequestRecord {
            model: "flash".into(),
            images: (0..100)
                .map(|index| crate::image::ImageDelivery {
                    observation_id: Some(format!("observation-{index}")),
                    message_index: index,
                    block_index: 0,
                    call_id: None,
                    image: "sha256:asset".into(),
                    included: false,
                    reason: Some("界\t\"\u{1}".repeat(2000)),
                })
                .collect(),
        });
        record.turns.push(turn);
        record.limit_image_reports();
        let images = &record.turns[0].image_requests[0].images;
        assert!(!images.is_empty() && images.len() < 100);
        assert_eq!(
            images.last().unwrap().observation_id.as_deref(),
            Some("observation-99")
        );
        assert_eq!(
            record.turns[0].image_reports_omitted as usize,
            100 - images.len()
        );
        assert!(serde_json::to_vec_pretty(&record).unwrap().len() < IMAGE_REPORT_BUDGET_BYTES);
        let once = record.clone();
        record.limit_image_reports();
        assert_eq!(
            record, once,
            "saving twice must not remove/count the same reports twice"
        );
        record.turns[0].image_requests.push(ImageRequestRecord {
            model: "pro".into(),
            images: vec![crate::image::ImageDelivery {
                observation_id: Some("oversized".into()),
                message_index: 0,
                block_index: 0,
                call_id: None,
                image: "asset".into(),
                included: false,
                reason: Some("x".repeat(IMAGE_REPORT_BUDGET_BYTES * 2)),
            }],
        });
        record.limit_image_reports();
        assert_eq!(
            record.turns[0].image_reports_omitted,
            once.turns[0].image_reports_omitted + 1
        );
        assert!(serde_json::to_vec_pretty(&record).unwrap().len() < IMAGE_REPORT_BUDGET_BYTES);
    }

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
