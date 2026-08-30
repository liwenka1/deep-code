use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::migrate::{SessionRecordV1, migrate_v1};
use super::{
    SESSION_SCHEMA_VERSION, SessionId, SessionRecord, SessionStore, SessionStoreError,
    sessions_dir_for_workspace, validate_session_id,
};

/// `<workspace>/.deep-code` and the `sessions` directory inside it — the two
/// levels deep-code owns here. Above them the path is the user's.
const OWNED_STATE_DIRS: usize = 2;

/// JSON file backend: one pretty-printed file per session.
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    root: PathBuf,
}

impl JsonSessionStore {
    pub fn for_workspace(workspace: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let root = sessions_dir_for_workspace(workspace.as_ref());
        // `.deep-code` and `sessions` under it are both ours, so both must be
        // real directories. Plain `create_dir_all` follows a symlink at either
        // level, and a repository shipping `.deep-code` as a link then
        // relocated every session transcript outside the workspace. That is the
        // same escape `write_self_ignore` closes one level down at the leaf —
        // which buys nothing if the directory holding the leaf was already
        // redirected.
        crate::paths::ensure_owned_dirs(&root, OWNED_STATE_DIRS).map_err(|error| {
            SessionStoreError::Io {
                message: format!("failed to create {}: {error}", root.display()),
            }
        })?;
        if let Some(state_dir) = root.parent() {
            Self::write_self_ignore(state_dir);
        }
        Ok(Self { root })
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
    fn write_self_ignore(state_dir: &Path) {
        const BODY: &str = "# Written by deep-code: this directory holds session transcripts and logs.\n\
             # Deleting this file does not opt out — deep-code writes it back next session.\n\
             # To commit this directory after all, edit this file instead (e.g. remove the\n\
             # `*` line); deep-code never touches an existing .gitignore.\n\
             *\n";
        let marker = state_dir.join(".gitignore");
        let _ = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .and_then(|mut file| std::io::Write::write_all(&mut file, BODY.as_bytes()));
    }

    fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), SessionStoreError> {
        let parent = path.parent().ok_or_else(|| SessionStoreError::Io {
            message: format!("session path has no parent: {}", path.display()),
        })?;
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("session");
        // pid + nanos, not milliseconds: two processes sharing one workspace
        // (`deep-code -c` in two terminals, or a TUI alongside `serve --resume`)
        // each run their own save loop. On a millisecond-only name their staging
        // files collide, the second `File::create` truncates the first mid-write,
        // and a half-written JSON can get renamed over the live session — which
        // then fails to parse and reads to the user as "my session disappeared".
        let tmp_path = parent.join(format!(
            ".{stem}.{}.{}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let write_result = (|| {
            let mut file = File::create(&tmp_path).map_err(|error| SessionStoreError::Io {
                message: format!("failed to create {}: {error}", tmp_path.display()),
            })?;
            file.write_all(contents)
                .map_err(|error| SessionStoreError::Io {
                    message: format!("failed to write {}: {error}", tmp_path.display()),
                })?;
            file.sync_all().map_err(|error| SessionStoreError::Io {
                message: format!("failed to fsync {}: {error}", tmp_path.display()),
            })?;
            Ok(())
        })();
        // Never leave staging files behind on a failed save: they are invisible
        // to `list` (dot-prefixed, no `.json`) so nothing would ever clean them.
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        if let Err(error) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(SessionStoreError::Io {
                message: format!(
                    "failed to rename {} -> {}: {error}",
                    tmp_path.display(),
                    path.display()
                ),
            });
        }
        sync_directory(parent)?;
        Ok(())
    }
}

fn sync_directory(path: &Path) -> Result<(), SessionStoreError> {
    #[cfg(unix)]
    {
        let dir = File::open(path).map_err(|error| SessionStoreError::Io {
            message: format!("failed to open directory {}: {error}", path.display()),
        })?;
        dir.sync_all().map_err(|error| SessionStoreError::Io {
            message: format!("failed to fsync directory {}: {error}", path.display()),
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

impl JsonSessionStore {
    fn read_file(&self, path: &Path) -> Result<SessionRecord, SessionStoreError> {
        let raw = fs::read_to_string(path).map_err(|error| {
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
        Self::write_atomic(&path, json.as_bytes())
    }

    fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
        self.read_file(&self.path_for(id)?)
    }

    fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        let mut records = Vec::new();
        let entries = fs::read_dir(&self.root).map_err(|error| SessionStoreError::Io {
            message: format!("failed to read {}: {error}", self.root.display()),
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| SessionStoreError::Io {
                message: format!("failed to read session directory entry: {error}"),
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str())
                && validate_session_id(stem).is_err()
            {
                continue;
            }
            match self.read_file(&path) {
                Ok(record) => records.push(record),
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
        if !path.is_file() {
            return Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            });
        }
        fs::remove_file(&path).map_err(|error| SessionStoreError::Io {
            message: format!("failed to delete {}: {error}", path.display()),
        })
    }
}

#[cfg(test)]
mod tests;
