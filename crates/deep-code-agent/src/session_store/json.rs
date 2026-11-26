use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
#[cfg(test)]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    MAX_SESSION_BODY_BYTES, MAX_SESSION_ENTRIES, MAX_SESSION_FILE_BYTES, MAX_SESSION_LIST,
    PreparedSession, SessionId, SessionRecord, SessionStore, SessionStoreError,
    sessions_dir_for_workspace, validate_session_id,
};

mod archive;
mod chunks;
mod lock;
pub(crate) use lock::StoreLock;

/// Bound diagnostics as well as retained records while scanning the directory.
const MAX_SESSION_DIAGNOSTICS: usize = 32;

#[derive(Default)]
struct SessionWindow {
    records: Vec<(SessionRecord, usize)>,
    newest_excluded: Option<(u64, SessionId)>,
}

impl SessionWindow {
    fn push(&mut self, record: SessionRecord, bytes: usize) {
        let key = (record.updated_at_ms, record.id.clone());
        if self
            .newest_excluded
            .as_ref()
            .is_some_and(|cutoff| key <= *cutoff)
        {
            return;
        }
        self.records.push((record, bytes));
        self.records.sort_by(|(left, _), (right, _)| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        let mut remaining = MAX_SESSION_BODY_BYTES as usize;
        let mut keep = 0;
        for (_, bytes) in self.records.iter().take(MAX_SESSION_LIST) {
            if *bytes > remaining {
                break;
            }
            remaining -= bytes;
            keep += 1;
        }
        if let Some((excluded, _)) = self.records.get(keep) {
            // Keep a continuous recent prefix. Remember its frontier so later
            // directory entries cannot fill holes with previously excluded history.
            self.newest_excluded = Some((excluded.updated_at_ms, excluded.id.clone()));
        }
        self.records.truncate(keep);
    }
}

/// JSON backend with bounded manifests and immutable chunks for large records.
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    root: PathBuf,
    workspace: PathBuf,
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
            workspace: workspace.to_path_buf(),
            directory: Arc::new(directory),
            diagnostics: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    pub(super) fn workspace_path(&self) -> &Path {
        &self.workspace
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

    fn reclaim_chunks(&self) {
        if let Err(error) = chunks::collect(&self.directory) {
            self.diagnose(format!("session chunk cleanup skipped: {error}"));
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
        write_atomic_in(&self.directory, filename, contents).map_err(|error| {
            SessionStoreError::Io {
                message: format!("failed to save {}: {error}", path.display()),
            }
        })
    }
}

fn write_atomic_in(
    directory: &Dir,
    filename: &std::ffi::OsStr,
    contents: &[u8],
) -> std::io::Result<()> {
    let temporary = format!(".{}.tmp", uuid::Uuid::new_v4());
    let saved = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = directory.open_with(&temporary, &options)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        directory.rename(&temporary, directory, filename)?;
        #[cfg(unix)]
        directory.open(".")?.sync_all()?;
        Ok(())
    })();
    if saved.is_err() {
        let _ = directory.remove_file(&temporary);
    }
    saved
}

fn store_io(error: std::io::Error) -> SessionStoreError {
    SessionStoreError::Io {
        message: error.to_string(),
    }
}

fn read_index(directory: &Dir, filename: &std::ffi::OsStr) -> std::io::Result<(String, bool)> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = directory.open_with(filename, &options)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("session entry is not a regular file"));
    }
    let mut contents = String::new();
    file.take(MAX_SESSION_FILE_BYTES.saturating_add(1))
        .read_to_string(&mut contents)?;
    let too_large = contents.len() as u64 > MAX_SESSION_FILE_BYTES;
    Ok((contents, too_large))
}

impl JsonSessionStore {
    #[cfg(test)]
    fn read_file(&self, path: &Path) -> Result<SessionRecord, SessionStoreError> {
        self.read_sized(path, None).map(|(record, _)| record)
    }

    fn read_sized(
        &self,
        path: &Path,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(SessionRecord, usize), SessionStoreError> {
        let _guard = StoreLock::acquire(&self.directory, false, cancel)?;
        self.read_while_locked(path, cancel)
    }

    fn read_while_locked(
        &self,
        path: &Path,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(SessionRecord, usize), SessionStoreError> {
        let filename = path.file_name().ok_or_else(|| SessionStoreError::Io {
            message: "invalid session filename".into(),
        })?;
        let (raw, too_large) = read_index(&self.directory, filename).map_err(|error| {
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
        let mut value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| SessionStoreError::Serialization {
                message: format!("failed to parse {}: {error}", path.display()),
            })?;
        let body_bytes = if value.get("storage").is_some() {
            let body = chunks::read(&self.directory, value, path, cancel)?;
            let bytes = body.len();
            value = serde_json::from_slice(&body).map_err(|error| {
                SessionStoreError::Serialization {
                    message: format!("failed to parse reconstructed {}: {error}", path.display()),
                }
            })?;
            bytes
        } else {
            raw.len()
        };
        let mut record = serde_json::from_value::<SessionRecord>(value).map_err(|error| {
            SessionStoreError::Serialization {
                message: format!("failed to parse {}: {error}", path.display()),
            }
        })?;
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
        Ok((record, body_bytes))
    }
}

impl JsonSessionStore {
    /// Synchronous, cancellable load of a single session, including waiting
    /// for concurrent writers. UI callers must run this on a worker.
    pub fn load_cancellable(
        &self,
        id: &SessionId,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<SessionRecord, SessionStoreError> {
        check_scan_cancelled(cancel)?;
        let (record, _) = self.load_sized(id, Some(cancel))?;
        check_scan_cancelled(cancel)?;
        Ok(record)
    }

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
        let mut resumable = SessionWindow::default();
        let mut empty = SessionWindow::default();
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
            let record = self.load_sized(&SessionId(stem.to_string()), Some(cancel));
            check_scan_cancelled(cancel)?;
            match record {
                Ok((record, bytes)) => {
                    let target = if record.has_user_entry() {
                        &mut resumable
                    } else {
                        &mut empty
                    };
                    target.push(record, bytes);
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
        empty
            .records
            .truncate(MAX_SESSION_LIST.saturating_sub(resumable.records.len()));
        resumable.records.extend(empty.records);
        let mut records: Vec<_> = resumable
            .records
            .into_iter()
            .map(|(record, _)| record)
            .collect();
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
        if json.len() as u64 > MAX_SESSION_BODY_BYTES {
            return Err(SessionStoreError::TooLarge {
                path: path.display().to_string(),
                bytes: json.len() as u64,
                limit: MAX_SESSION_BODY_BYTES,
            });
        }
        if session.entry_count > MAX_SESSION_ENTRIES {
            return Err(SessionStoreError::TooManyEntries {
                path: path.display().to_string(),
                count: session.entry_count,
                limit: MAX_SESSION_ENTRIES,
            });
        }
        let _guard = StoreLock::acquire(&self.directory, true, None)?;
        self.reclaim_chunks();
        if json.len() as u64 > MAX_SESSION_FILE_BYTES {
            let manifest = chunks::write(&self.directory, json.as_bytes())?;
            self.write_atomic(&path, &manifest)?;
        } else {
            self.write_atomic(&path, json.as_bytes())?;
        }
        // Cleanup failure does not undo an already durable snapshot.
        self.reclaim_chunks();
        Ok(())
    }

    fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
        self.load_sized(id, None).map(|(record, _)| record)
    }

    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        self.list_cancellable(&tokio_util::sync::CancellationToken::new())
    }

    fn delete(&self, id: &SessionId) -> Result<(), SessionStoreError> {
        let path = self.path_for(id)?;
        let _guard = StoreLock::acquire(&self.directory, true, None)?;
        if !self
            .directory
            .symlink_metadata(path.file_name().expect("validated filename"))
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            });
        }
        let removed = self
            .read_while_locked(&path, None)
            .ok()
            .map(|(record, _)| record);
        self.directory
            .remove_file(path.file_name().expect("validated filename"))
            .map_err(|error| SessionStoreError::Io {
                message: format!("failed to delete {}: {error}", path.display()),
            })?;
        #[cfg(unix)]
        self.directory
            .open(".")
            .and_then(|file| file.sync_all())
            .map_err(store_io)?;
        if let Some(record) = removed
            && let Err(error) = self.reclaim_deleted_archive(&record)
        {
            self.diagnose(format!("transcript archive cleanup skipped: {error}"));
        }
        self.reclaim_chunks();
        Ok(())
    }
}

impl JsonSessionStore {
    fn load_sized(
        &self,
        id: &SessionId,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(SessionRecord, usize), SessionStoreError> {
        let (mut record, bytes) = self.read_sized(&self.path_for(id)?, cancel)?;
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
        Ok((record, bytes))
    }
}

#[cfg(test)]
mod tests;
