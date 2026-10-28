use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
#[cfg(test)]
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::migrate::{SessionRecordV1, migrate_v1};
use super::{
    SESSION_SCHEMA_VERSION, SessionId, SessionRecord, SessionStore, SessionStoreError,
    sessions_dir_for_workspace, validate_session_id,
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
            #[cfg(unix)]
            self.directory.try_clone()?.into_std_file().sync_all()?;
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
        let read = || -> std::io::Result<String> {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let mut file = self.directory.open_with(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("invalid session filename"))?,
                &options,
            )?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("session entry is not a regular file"));
            }
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            Ok(contents)
        };
        let raw = read().map_err(|error| {
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
        match version {
            // Legacy wire-message layout: migrate in memory; the next persist
            // writes the file back as v2.
            1 => {
                let v1: SessionRecordV1 = serde_json::from_value(value).map_err(parse_error)?;
                Ok(migrate_v1(v1))
            }
            SESSION_SCHEMA_VERSION => {
                serde_json::from_value::<SessionRecord>(value).map_err(parse_error)
            }
            other => Err(SessionStoreError::UnsupportedSchema {
                found: other,
                expected: SESSION_SCHEMA_VERSION,
            }),
        }
    }
}

impl SessionStore for JsonSessionStore {
    fn save_serialized(&self, id: &SessionId, json: &str) -> Result<(), SessionStoreError> {
        validate_session_id(id.as_str())?;
        let path = self.path_for(id)?;
        self.write_atomic(&path, json.as_bytes())
    }

    fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
        let mut record = self.read_file(&self.path_for(id)?)?;
        // The FILENAME is authoritative for the id, not the record's stored one.
        // A session file copied to a new name (a "fork") keeps the original's id
        // inside it; without this, the resumed runtime would save back under that
        // original id and silently clobber the source session. Binding the id to
        // the file it was loaded from makes a fork write to itself.
        record.id = id.clone();
        Ok(record)
    }

    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let mut records = Vec::new();
        let entries = self
            .directory
            .entries()
            .map_err(|error| SessionStoreError::Io {
                message: format!("failed to read {}: {error}", self.root.display()),
            })?;

        for entry in entries {
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
            match self.read_file(&path) {
                Ok(mut record) => {
                    // Bind the id to the filename, as `load` does and for the same
                    // reason: the picker and any resume off this list must target
                    // the file that actually holds the session, not an id copied
                    // inside it.
                    record.id = SessionId(stem.to_string());
                    records.push(record);
                }
                Err(SessionStoreError::UnsupportedSchema { .. }) => {
                    eprintln!("skipping unsupported session file {}", path.display());
                }
                Err(error) => {
                    eprintln!("skipping unreadable session {}: {error}", path.display());
                }
            }
        }

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
