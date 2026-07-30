use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::migrate::{SessionRecordV1, migrate_v1};
use super::{
    SESSION_SCHEMA_VERSION, SessionId, SessionRecord, SessionStore, SessionStoreError,
    sessions_dir_for_workspace, validate_session_id,
};

/// JSON file backend: one pretty-printed file per session.
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    root: PathBuf,
}

impl JsonSessionStore {
    pub fn for_workspace(workspace: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let root = sessions_dir_for_workspace(workspace.as_ref());
        fs::create_dir_all(&root).map_err(|error| SessionStoreError::Io {
            message: format!("failed to create {}: {error}", root.display()),
        })?;
        Ok(Self { root })
    }

    fn path_for(&self, id: &SessionId) -> Result<PathBuf, SessionStoreError> {
        validate_session_id(id.as_str())?;
        Ok(self.root.join(format!("{}.json", id.as_str())))
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
mod tests {
    use super::*;

    #[test]
    fn json_store_rejects_invalid_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        assert!(SessionId::parse("../evil").is_err());
        assert!(matches!(
            store.load(&SessionId("../evil".to_string())),
            Err(SessionStoreError::InvalidId { .. })
        ));
        assert!(matches!(
            store.delete(&SessionId("..".to_string())),
            Err(SessionStoreError::InvalidId { .. })
        ));
    }

    #[test]
    fn json_store_round_trips_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
        record.entries.push(std::sync::Arc::new(
            crate::session_entry::SessionEntry::user("hello"),
        ));
        record.touch();

        store.save(&record).unwrap();
        let loaded = store.load(&record.id).unwrap();

        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.entries.len(), 2);
        assert!(matches!(
            &loaded.entries[1].kind,
            crate::session_entry::EntryKind::User { content } if content == "hello"
        ));
    }

    #[test]
    fn json_store_migrates_v1_files_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        let v1_json = serde_json::json!({
            "schema_version": 1,
            "id": "session_1_0",
            "workspace": dir.path(),
            "created_at_ms": 1,
            "updated_at_ms": 2,
            "config": {
                "base_url": "https://api.deepseek.com/beta",
                "model": "deepseek-v4-pro",
                "timeout_secs": 60,
                "api_key_present": false
            },
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "", "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "shell", "arguments": "{}"}
                }]}
                // interrupted: no tool message — the migration leaves a
                // pending exchange instead of requiring a repair pass
            ],
            "turns": []
        });
        let path = dir
            .path()
            .join(".deep-code/sessions")
            .join("session_1_0.json");
        std::fs::write(&path, serde_json::to_string_pretty(&v1_json).unwrap()).unwrap();

        let loaded = store.load(&SessionId("session_1_0".to_string())).unwrap();
        assert_eq!(loaded.schema_version, super::SESSION_SCHEMA_VERSION);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.preview(), "hi");

        // Saving writes the file back as v2; reloading stays stable.
        store.save(&loaded).unwrap();
        let reloaded = store.load(&loaded.id).unwrap();
        assert_eq!(reloaded.entries, loaded.entries);
    }

    #[test]
    fn json_store_rejects_future_schema() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        let future = serde_json::json!({"schema_version": 3, "id": "session_9_0"});
        let path = dir
            .path()
            .join(".deep-code/sessions")
            .join("session_9_0.json");
        std::fs::write(&path, future.to_string()).unwrap();

        assert!(matches!(
            store.load(&SessionId("session_9_0".to_string())),
            Err(SessionStoreError::UnsupportedSchema { found: 3, .. })
        ));
    }

    #[test]
    fn json_store_list_sorts_by_updated_at_desc() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();

        let mut older = SessionRecord::new(dir.path().to_path_buf(), "system");
        older.updated_at_ms = 1;
        store.save(&older).unwrap();

        let mut newer = SessionRecord::new(dir.path().to_path_buf(), "system");
        newer.updated_at_ms = 2;
        store.save(&newer).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, newer.id);
    }

    #[test]
    fn json_store_delete_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        let record = SessionRecord::new(dir.path().to_path_buf(), "system");
        let id = record.id.clone();
        store.save(&record).unwrap();
        store.delete(&id).unwrap();
        assert!(matches!(
            store.load(&id),
            Err(SessionStoreError::NotFound { .. })
        ));
    }

    #[test]
    fn json_store_export_is_pretty_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
        let record = SessionRecord::new(dir.path().to_path_buf(), "system");
        store.save(&record).unwrap();
        let exported = store.export(&record.id).unwrap();
        assert!(exported.contains("\"schema_version\""));
        assert!(exported.contains('\n'));
    }
}
