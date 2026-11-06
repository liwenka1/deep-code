use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    MAX_SESSION_ENTRIES, MAX_SESSION_FILE_BYTES, MAX_SESSION_LIST, PreparedSession, SessionId,
    SessionRecord, SessionStore, SessionStoreError, sessions_dir_for_workspace,
    validate_session_id,
};

/// Schemas written before turn and entry identities became part of a session.
/// Schema 2 was the last published format and had no full-record tag; schema 3
/// was the short-lived authenticated format on the development branch. Keep
/// these wire structs local to the loader so the current model can remain
/// strict while both older shapes are migrated explicitly.
const LEGACY_SESSION_SCHEMA_VERSION: u32 = 3;
const PUBLISHED_SESSION_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTurnRecord {
    started_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyCheckpointRecord {
    id: crate::checkpoint::CheckpointId,
    label: String,
    created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySessionEntry {
    #[serde(flatten)]
    kind: crate::session_entry::EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySessionRecord {
    schema_version: u32,
    id: SessionId,
    workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extra_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra_roots_mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record_mac: Option<String>,
    created_at_ms: u64,
    updated_at_ms: u64,
    entries: Vec<Arc<LegacySessionEntry>>,
    turns: Vec<LegacyTurnRecord>,
    #[serde(default)]
    checkpoints: Vec<LegacyCheckpointRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction: Option<String>,
    #[serde(default)]
    session_cost: crate::pricing::CostEstimate,
    #[serde(default)]
    session_cache_hit_tokens: u64,
    #[serde(default)]
    session_cache_miss_tokens: u64,
    #[serde(default)]
    session_cache_savings: crate::pricing::CostEstimate,
}

impl LegacySessionRecord {
    fn migrate(mut self) -> Result<SessionRecord, SessionStoreError> {
        if !matches!(
            self.schema_version,
            PUBLISHED_SESSION_SCHEMA_VERSION | LEGACY_SESSION_SCHEMA_VERSION
        ) {
            return Err(SessionStoreError::Serialization {
                message: format!(
                    "unsupported legacy session schema version {}",
                    self.schema_version
                ),
            });
        }

        // Both legacy shapes must carry a host-authenticated tag before any
        // defaults or identities are added. Schema 2 was published before
        // full-record authentication, so files from that release without a
        // tag cannot be safely resumed or upgraded: a model-written file could
        // otherwise smuggle an altered transcript through this migration.
        let tag = self.record_mac.take();
        let authenticated = crate::session_integrity::verify_serialized(&self, tag.as_deref());
        self.record_mac = tag;
        if !authenticated {
            return Err(SessionStoreError::AuthenticationFailed {
                path: "legacy session".to_string(),
            });
        }

        let turn_ids: Vec<_> = self
            .turns
            .iter()
            .map(|_| crate::runtime::TurnId::new())
            .collect();
        let mut current_turn = None;
        let mut next_turn = 0;
        let entries = self
            .entries
            .into_iter()
            .map(|entry| {
                let turn_id = match entry.kind {
                    crate::session_entry::EntryKind::User { .. } => {
                        let id = turn_ids
                            .get(next_turn)
                            .cloned()
                            .unwrap_or_else(crate::runtime::TurnId::new);
                        next_turn = next_turn.saturating_add(1);
                        current_turn = Some(id.clone());
                        Some(id)
                    }
                    crate::session_entry::EntryKind::Assistant { .. } => current_turn.clone(),
                    _ => None,
                };
                Arc::new(crate::session_entry::SessionEntry {
                    turn_id,
                    kind: entry.kind.clone(),
                })
            })
            .collect();
        let turns = self
            .turns
            .into_iter()
            .zip(turn_ids.iter().cloned())
            .map(|(turn, id)| super::TurnRecord {
                id,
                started_at_ms: turn.started_at_ms,
            })
            .collect();
        let checkpoints = self
            .checkpoints
            .into_iter()
            .enumerate()
            .map(|(index, checkpoint)| {
                let turn_id = turn_ids
                    .get(index)
                    .cloned()
                    .or_else(|| turn_ids.last().cloned())
                    .unwrap_or_else(crate::runtime::TurnId::new);
                super::CheckpointRecord {
                    id: checkpoint.id,
                    turn_id,
                    label: checkpoint.label,
                    created_at_ms: checkpoint.created_at_ms,
                }
            })
            .collect();

        let mut record = SessionRecord {
            id: self.id,
            workspace: self.workspace,
            extra_roots: self.extra_roots,
            extra_roots_mac: self.extra_roots_mac,
            record_mac: None,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            entries,
            turns,
            checkpoints,
            summary: self.summary,
            compaction: self.compaction,
            session_cost: self.session_cost,
            session_cache_hit_tokens: self.session_cache_hit_tokens,
            session_cache_miss_tokens: self.session_cache_miss_tokens,
            session_cache_savings: self.session_cache_savings,
        };
        record.record_mac = Some(
            crate::session_integrity::sign_record(&mut record).ok_or_else(|| {
                SessionStoreError::KeyUnavailable {
                    detail: "could not sign migrated session".to_string(),
                }
            })?,
        );
        Ok(record)
    }
}

/// Bound diagnostics as well as retained records while scanning the directory.
const MAX_SESSION_DIAGNOSTICS: usize = 32;

/// JSON file backend: one pretty-printed file per session.
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    root: PathBuf,
    directory: Arc<Dir>,
    /// A bounded sample of records `list()` refused to return, one per line.
    /// Collected instead of printed: library stderr cannot be shown by the
    /// caller that asked for the list, and in the TUI it was swallowed by the
    /// log redirect — leaving "No saved sessions." to explain a directory full
    /// of files.
    diagnostics: Arc<std::sync::Mutex<Vec<String>>>,
}

impl JsonSessionStore {
    pub fn for_workspace(workspace: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let canonical = crate::paths::canonicalize(workspace.as_ref()).map_err(store_io)?;
        let mut store = Self::for_pinned_workspace(&canonical)?;
        store.root = sessions_dir_for_workspace(workspace.as_ref());
        Ok(store)
    }

    /// Reuse an approved canonical root without following a replacement link.
    pub fn for_pinned_workspace(workspace: &Path) -> Result<Self, SessionStoreError> {
        let root = crate::pinned_dir::open_canonical(workspace).map_err(store_io)?;
        let state = crate::pinned_dir::owned_child(&root, ".deep-code").map_err(store_io)?;
        let directory = crate::pinned_dir::owned_child(&state, "sessions").map_err(store_io)?;
        Self::write_self_ignore(&state);
        Ok(Self {
            root: sessions_dir_for_workspace(workspace),
            directory: Arc::new(directory),
            diagnostics: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    /// Take the lines accumulated by earlier [`SessionStore::list`] calls:
    /// sessions skipped because they could not be trusted or read. A final
    /// line reports when further diagnostics were omitted. Empty is normal.
    #[must_use]
    pub fn take_diagnostics(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .diagnostics
                .lock()
                .expect("session diagnostics lock poisoned"),
        )
    }

    fn diagnose(&self, message: String) {
        let mut diagnostics = self
            .diagnostics
            .lock()
            .expect("session diagnostics lock poisoned");
        if diagnostics.len() < MAX_SESSION_DIAGNOSTICS {
            diagnostics.push(message);
        } else if diagnostics.len() == MAX_SESSION_DIAGNOSTICS {
            diagnostics.push("additional session diagnostics omitted".to_string());
        }
    }

    fn path_for(&self, id: &SessionId) -> Result<PathBuf, SessionStoreError> {
        validate_session_id(id.as_str())?;
        Ok(self.root.join(format!("{}.json", id.as_str())))
    }

    /// Drop a `.gitignore` containing `*` into the state directory so the agent's
    /// own bookkeeping cannot be committed into the user's repository.
    ///
    /// `.deep-code/` is created inside the workspace and holds full conversation
    /// transcripts plus the log; nothing kept it out of git, so a `git add -A`
    /// (by the user, or by the agent itself) committed the transcripts. The eval
    /// harness already had to exclude the directory by hand, which is the same
    /// hazard noticed in one place and not fixed at the source.
    ///
    /// Best-effort and written only when absent: an existing file is never
    /// touched, whatever it says — which also means *deleting* it is not a way
    /// to opt out (the next session writes it back). The file itself says so,
    /// and names the edit that does opt out.
    ///
    /// "Absent" is decided by the create itself, not by a prior `exists()`.
    /// `Path::exists` FOLLOWS symlinks, so a *dangling* one answered "absent"
    /// and the plain `fs::write` that followed then created the link's target —
    /// anywhere on disk, from the unsandboxed parent process, triggered by
    /// nothing more than opening a repository that ships
    /// `.deep-code/.gitignore` as a symlink. That is the same defect
    /// `workspace_policy::resolve_for_write` was fixed for ("a link that exists
    /// is a link whether or not its target does"); this was its unaudited
    /// sibling. `create_new` asks the kernel the question atomically, which
    /// also closes the exists→write gap.
    fn write_self_ignore(state_dir: &Dir) {
        let _ = state_dir
            .open_with(
                ".gitignore",
                OpenOptions::new().write(true).create_new(true),
            )
            .and_then(|mut file| file.write_all(super::SELF_IGNORE_BODY.as_bytes()));
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), SessionStoreError> {
        let filename = path.file_name().ok_or_else(|| SessionStoreError::Io {
            message: "session path has no filename".into(),
        })?;
        let temporary = format!(".{}.tmp", uuid::Uuid::new_v4());
        let saved = (|| -> std::io::Result<()> {
            let mut file = self
                .directory
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            self.directory
                .rename(&temporary, &self.directory, filename)?;
            // Linux capability directories use O_PATH, which fsync rejects.
            // Reopen this same pinned directory for reading before syncing it.
            #[cfg(unix)]
            self.directory.open(".")?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = saved {
            let _ = self.directory.remove_file(&temporary);
            return Err(SessionStoreError::Io {
                message: format!("failed to save {}: {error}", path.display()),
            });
        }
        Ok(())
    }
}

fn store_io(error: std::io::Error) -> SessionStoreError {
    SessionStoreError::Io {
        message: error.to_string(),
    }
}

impl JsonSessionStore {
    fn read_file(&self, path: &Path) -> Result<SessionRecord, SessionStoreError> {
        let filename = path.file_name().ok_or_else(|| SessionStoreError::Io {
            message: "invalid session filename".into(),
        })?;
        let read = || -> std::io::Result<(String, bool)> {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = self.directory.open_with(filename, &options)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("session entry is not a regular file"));
            }
            let mut contents = String::new();
            // Read at most one byte beyond the limit so a file that grows
            // between metadata and read cannot allocate without bound.
            file.take(MAX_SESSION_FILE_BYTES.saturating_add(1))
                .read_to_string(&mut contents)?;
            let too_large = contents.len() as u64 > MAX_SESSION_FILE_BYTES;
            Ok((contents, too_large))
        };
        let (raw, too_large) = read().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionStoreError::NotFound {
                    id: path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("<unknown>")
                        .to_string(),
                }
            } else {
                SessionStoreError::Io {
                    message: format!("failed to read {}: {error}", path.display()),
                }
            }
        })?;
        if too_large {
            return Err(SessionStoreError::TooLarge {
                path: path.display().to_string(),
                bytes: MAX_SESSION_FILE_BYTES.saturating_add(1),
                limit: MAX_SESSION_FILE_BYTES,
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| SessionStoreError::Serialization {
                message: format!("failed to parse {}: {error}", path.display()),
            })?;
        let mut record = if value.get("schema_version").is_some() {
            let legacy: LegacySessionRecord = serde_json::from_value(value).map_err(|error| {
                SessionStoreError::Serialization {
                    message: format!("failed to parse {}: {error}", path.display()),
                }
            })?;
            legacy.migrate().map_err(|error| match error {
                SessionStoreError::AuthenticationFailed { .. } => {
                    SessionStoreError::AuthenticationFailed {
                        path: path.display().to_string(),
                    }
                }
                other => other,
            })?
        } else {
            serde_json::from_value::<SessionRecord>(value).map_err(|error| {
                SessionStoreError::Serialization {
                    message: format!("failed to parse {}: {error}", path.display()),
                }
            })?
        };
        if record.entries.len() > MAX_SESSION_ENTRIES {
            return Err(SessionStoreError::TooManyEntries {
                path: path.display().to_string(),
                count: record.entries.len(),
                limit: MAX_SESSION_ENTRIES,
            });
        }
        crate::session_integrity::ensure_key().map_err(|reason| {
            SessionStoreError::KeyUnavailable {
                detail: reason.to_string(),
            }
        })?;
        if !crate::session_integrity::verify_record(&mut record) {
            return Err(SessionStoreError::AuthenticationFailed {
                path: path.display().to_string(),
            });
        }
        Ok(record)
    }
}

impl JsonSessionStore {
    /// Synchronous, cancellable scan of every candidate. UI callers must run
    /// this on a worker and cancel when its result is no longer needed.
    /// Cancellation is checked between size-limited files; a partial list is
    /// never returned.
    pub fn list_cancellable(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<SessionRecord>, SessionStoreError> {
        check_scan_cancelled(cancel)?;
        // Keep the bounded window useful for both the session list and resume:
        // empty sessions may fill the newest slots, but an older non-empty
        // session must remain available to `-c` and `/resume`.
        // Directory order is unrelated to the signed update timestamps. Scan
        // every candidate before choosing the window: stopping after a fixed
        // number of entries or bytes can silently resume an older session.
        // Stream one size-limited record at a time and bound both the retained
        // windows and diagnostics, so memory does not grow with directory size.
        let mut resumable = Vec::new();
        let mut empty = Vec::new();
        let entries = self
            .directory
            .entries()
            .map_err(|error| SessionStoreError::Io {
                message: format!("failed to read {}: {error}", self.root.display()),
            })?;

        for entry in entries {
            check_scan_cancelled(cancel)?;
            let entry = entry.map_err(|error| SessionStoreError::Io {
                message: format!("failed to read session directory entry: {error}"),
            })?;
            let path = self.root.join(entry.file_name());
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_session_id(stem).is_err() {
                continue;
            }
            let record = self.load(&SessionId(stem.to_string()));
            check_scan_cancelled(cancel)?;
            match record {
                Ok(record) => {
                    let target = if record.has_user_entry() {
                        &mut resumable
                    } else {
                        &mut empty
                    };
                    if target.len() < MAX_SESSION_LIST {
                        target.push(record);
                    } else if let Some((oldest_index, oldest)) = target
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, candidate)| candidate.updated_at_ms)
                        && record.updated_at_ms > oldest.updated_at_ms
                    {
                        target[oldest_index] = record;
                    }
                }
                Err(error) => {
                    self.diagnose(format!(
                        "skipping unreadable session {}: {error}",
                        path.display()
                    ));
                }
            }
        }

        check_scan_cancelled(cancel)?;
        resumable.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        empty.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        resumable.truncate(MAX_SESSION_LIST);
        empty.truncate(MAX_SESSION_LIST.saturating_sub(resumable.len()));
        resumable.extend(empty);
        let mut records = resumable;
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        Ok(records)
    }
}

fn check_scan_cancelled(
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), SessionStoreError> {
    if cancel.is_cancelled() {
        Err(SessionStoreError::Cancelled)
    } else {
        Ok(())
    }
}

impl SessionStore for JsonSessionStore {
    fn save_prepared(&self, session: &PreparedSession) -> Result<(), SessionStoreError> {
        let id = session.id();
        let json = session.json();
        validate_session_id(id.as_str())?;
        let path = self.path_for(id)?;
        if json.len() as u64 > MAX_SESSION_FILE_BYTES {
            return Err(SessionStoreError::TooLarge {
                path: path.display().to_string(),
                bytes: json.len() as u64,
                limit: MAX_SESSION_FILE_BYTES,
            });
        }
        if session.entry_count > MAX_SESSION_ENTRIES {
            return Err(SessionStoreError::TooManyEntries {
                path: path.display().to_string(),
                count: session.entry_count,
                limit: MAX_SESSION_ENTRIES,
            });
        }
        self.write_atomic(&path, json.as_bytes())
    }

    fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
        let mut record = self.read_file(&self.path_for(id)?)?;
        // The FILENAME is authoritative for the id, not the record's stored one.
        // A session file copied to a new name (a "fork") keeps the original's id
        // inside it; without this, the resumed runtime would save back under that
        // original id and silently clobber the source session. Binding the id to
        // the file it was loaded from makes a fork write to itself.
        // A deliberately copied session file is a valid fork when its
        // original contents were host-authenticated: rebind the id and re-tag
        // the transcript, so the resumed runtime can still verify it. The
        // narrow grant tag is NOT re-issued — it names the id and workspace it
        // was approved under, so a fork keeps its conversation and loses its
        // authority (`runtime_launch` drops the grants, with a warning).
        if record.id != *id {
            record.id = id.clone();
            record.record_mac = Some(
                crate::session_integrity::sign_record(&mut record).ok_or_else(|| {
                    SessionStoreError::Serialization {
                        message: "could not authenticate the forked session".to_string(),
                    }
                })?,
            );
        }
        Ok(record)
    }

    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        self.list_cancellable(&tokio_util::sync::CancellationToken::new())
    }

    fn delete(&self, id: &SessionId) -> Result<(), SessionStoreError> {
        let path = self.path_for(id)?;
        if !self
            .directory
            .symlink_metadata(path.file_name().expect("validated filename"))
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            });
        }
        self.directory
            .remove_file(path.file_name().expect("validated filename"))
            .map_err(|error| SessionStoreError::Io {
                message: format!("failed to delete {}: {error}", path.display()),
            })
    }
}

#[cfg(test)]
mod tests;
