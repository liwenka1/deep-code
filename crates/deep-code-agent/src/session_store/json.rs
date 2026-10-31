use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
#[cfg(test)]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    MAX_SESSION_ENTRIES, MAX_SESSION_FILE_BYTES, MAX_SESSION_LIST, MAX_SESSION_SCAN,
    MAX_SESSION_SCAN_BYTES, SESSION_SCHEMA_VERSION, SessionId, SessionRecord, SessionStore,
    SessionStoreError, sessions_dir_for_workspace, validate_session_id,
};

/// JSON file backend: one pretty-printed file per session.
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    root: PathBuf,
    directory: Arc<Dir>,
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
        })
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
        let parse_error = |error: serde_json::Error| SessionStoreError::Serialization {
            message: format!("failed to parse {}: {error}", path.display()),
        };
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let record = match version {
            SESSION_SCHEMA_VERSION => {
                serde_json::from_value::<SessionRecord>(value).map_err(parse_error)?
            }
            other => {
                return Err(SessionStoreError::UnsupportedSchema {
                    found: other,
                    expected: SESSION_SCHEMA_VERSION,
                });
            }
        };
        if record.entries.len() > MAX_SESSION_ENTRIES {
            return Err(SessionStoreError::TooManyEntries {
                path: path.display().to_string(),
                count: record.entries.len(),
                limit: MAX_SESSION_ENTRIES,
            });
        }
        Ok(record)
    }
}

impl SessionStore for JsonSessionStore {
    fn save_serialized(&self, id: &SessionId, json: &str) -> Result<(), SessionStoreError> {
        validate_session_id(id.as_str())?;
        let path = self.path_for(id)?;
        if json.len() as u64 > MAX_SESSION_FILE_BYTES {
            return Err(SessionStoreError::TooLarge {
                path: path.display().to_string(),
                bytes: json.len() as u64,
                limit: MAX_SESSION_FILE_BYTES,
            });
        }
        let record: SessionRecord =
            serde_json::from_str(json).map_err(|error| SessionStoreError::Serialization {
                message: format!("failed to parse {}: {error}", path.display()),
            })?;
        if record.entries.len() > MAX_SESSION_ENTRIES {
            return Err(SessionStoreError::TooManyEntries {
                path: path.display().to_string(),
                count: record.entries.len(),
                limit: MAX_SESSION_ENTRIES,
            });
        }
        if record.schema_version != SESSION_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedSchema {
                found: record.schema_version,
                expected: SESSION_SCHEMA_VERSION,
            });
        }
        if record.id != *id {
            return Err(SessionStoreError::Serialization {
                message: format!(
                    "session id {} does not match destination {}",
                    record.id.as_str(),
                    id.as_str()
                ),
            });
        }
        if !crate::session_integrity::verify_record(&record) {
            return Err(SessionStoreError::AuthenticationUnavailable {
                id: id.as_str().to_string(),
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
        let authenticated = crate::session_integrity::verify_record(&record);
        record.id = id.clone();
        // A deliberately copied session file is a valid fork when its
        // original contents were host-authenticated. Rebind the id and issue
        // a fresh in-memory tag so the resumed runtime can still verify it.
        if authenticated {
            record.record_mac = crate::session_integrity::sign_record(&record);
        }
        Ok(record)
    }

    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        // Keep the bounded window useful for both the session list and resume:
        // empty sessions may fill the newest slots, but an older non-empty
        // session must remain available to `-c` and `/resume`.
        let mut resumable = Vec::new();
        let mut empty = Vec::new();
        let mut scanned_bytes = 0_u64;
        let entries = self
            .directory
            .entries()
            .map_err(|error| SessionStoreError::Io {
                message: format!("failed to read {}: {error}", self.root.display()),
            })?;

        for (scanned, entry) in entries.enumerate() {
            if scanned >= MAX_SESSION_SCAN {
                break;
            }
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
            let file_bytes = self
                .directory
                .symlink_metadata(entry.file_name())
                .ok()
                .filter(|metadata| metadata.is_file())
                .map_or(0, |metadata| metadata.len());
            if scanned_bytes.saturating_add(file_bytes) > MAX_SESSION_SCAN_BYTES {
                continue;
            }
            scanned_bytes = scanned_bytes.saturating_add(file_bytes);
            match self.read_file(&path) {
                Ok(mut record) => {
                    if !crate::session_integrity::verify_record(&record) {
                        eprintln!(
                            "skipping unauthenticated session {} (modified file)",
                            path.display()
                        );
                        continue;
                    }
                    // Bind the id to the filename, as `load` does and for the same
                    // reason: the picker and any resume off this list must target
                    // the file that actually holds the session, not an id copied
                    // inside it.
                    record.id = SessionId(stem.to_string());
                    record.record_mac = crate::session_integrity::sign_record(&record);
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
                Err(SessionStoreError::UnsupportedSchema { .. }) => {
                    eprintln!("skipping unsupported session file {}", path.display());
                }
                Err(error) => {
                    eprintln!("skipping unreadable session {}: {error}", path.display());
                }
            }
        }

        resumable.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        empty.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        resumable.truncate(MAX_SESSION_LIST);
        empty.truncate(MAX_SESSION_LIST.saturating_sub(resumable.len()));
        resumable.extend(empty);
        let mut records = resumable;
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
        Ok(records)
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
