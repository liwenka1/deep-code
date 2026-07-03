use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::tool::ToolError;

const CHECKPOINT_DIR: &str = ".deep-code/checkpoints";
const SKIP_DIRS: &[&str] = &[".git", ".deep-code", "target", "node_modules"];
/// Default retention: snapshots beyond this count are pruned oldest-first.
/// Every turn creates before/after snapshots, so without a cap the storage
/// grows by two workspace copies per turn.
pub const DEFAULT_MAX_SNAPSHOTS: usize = 20;

/// Identifier for a workspace snapshot stored outside `.git`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckpointId(pub String);

/// Side-storage workspace snapshots (before/after turns).
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    workspace: PathBuf,
    storage_root: PathBuf,
    max_snapshots: usize,
}

impl CheckpointStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let workspace = workspace.into();
        let canonical = workspace
            .canonicalize()
            .map_err(|error| ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: format!(
                    "failed to resolve workspace root {}: {error}",
                    workspace.display()
                ),
            })?;
        let storage_root = canonical.join(CHECKPOINT_DIR);
        fs::create_dir_all(&storage_root).map_err(|error| ToolError::ExecutionFailed {
            name: "checkpoint".to_string(),
            message: format!(
                "failed to create checkpoint storage {}: {error}",
                storage_root.display()
            ),
        })?;
        Ok(Self {
            workspace: canonical,
            storage_root,
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
        })
    }

    /// Override the retention cap (0 disables pruning).
    #[must_use]
    pub fn with_max_snapshots(mut self, max_snapshots: usize) -> Self {
        self.max_snapshots = max_snapshots;
        self
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Capture a full workspace snapshot under side storage, then prune the
    /// oldest snapshots beyond the retention cap.
    pub fn snapshot(&self, label: &str) -> Result<CheckpointId, ToolError> {
        let id = format!(
            "{}_{}",
            sanitize_label(label),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis())
        );
        let dest = self.storage_root.join(&id);
        copy_tree(&self.workspace, &dest, true)?;
        self.prune_old_snapshots();
        Ok(CheckpointId(id))
    }

    /// Best-effort retention: delete the oldest snapshot directories (by the
    /// trailing millisecond timestamp in their names) beyond `max_snapshots`.
    /// Failures are logged, never propagated — a full disk must not fail the
    /// snapshot that just succeeded.
    fn prune_old_snapshots(&self) {
        if self.max_snapshots == 0 {
            return;
        }
        let Ok(mut ids) = self.list() else {
            return;
        };
        if ids.len() <= self.max_snapshots {
            return;
        }
        ids.sort_by_key(|id| snapshot_timestamp(&id.0));
        let excess = ids.len() - self.max_snapshots;
        for id in ids.into_iter().take(excess) {
            let path = self.storage_root.join(&id.0);
            if let Err(error) = fs::remove_dir_all(&path) {
                eprintln!("checkpoint prune failed for {}: {error}", id.0);
            }
        }
    }

    pub fn restore(&self, id: &CheckpointId) -> Result<(), ToolError> {
        let source = self.storage_root.join(&id.0);
        if !source.is_dir() {
            return Err(ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: format!("checkpoint '{}' does not exist", id.0),
            });
        }
        clear_workspace_contents(&self.workspace)?;
        copy_tree(&source, &self.workspace, false)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<CheckpointId>, ToolError> {
        let mut ids = Vec::new();
        let entries =
            fs::read_dir(&self.storage_root).map_err(|error| ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: format!("failed to list checkpoints: {error}"),
            })?;
        for entry in entries {
            let entry = entry.map_err(|error| ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: format!("failed to read checkpoint entry: {error}"),
            })?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                ids.push(CheckpointId(
                    entry.file_name().to_string_lossy().into_owned(),
                ));
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// Trailing `_{ms}` component of a snapshot directory name; unparseable names
/// sort as oldest so malformed directories are pruned first.
fn snapshot_timestamp(id: &str) -> u128 {
    id.rsplit('_')
        .next()
        .and_then(|tail| tail.parse::<u128>().ok())
        .unwrap_or(0)
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn should_skip(rel: &Path) -> bool {
    rel.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| SKIP_DIRS.contains(&part))
    })
}

fn copy_tree(source: &Path, dest: &Path, skip_meta: bool) -> Result<(), ToolError> {
    fs::create_dir_all(dest).map_err(|error| checkpoint_error("create snapshot dir", error))?;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        let rel =
            entry
                .path()
                .strip_prefix(source)
                .map_err(|error| ToolError::ExecutionFailed {
                    name: "checkpoint".to_string(),
                    message: error.to_string(),
                })?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if skip_meta && should_skip(rel) {
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| checkpoint_error("create snapshot subdir", error))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| checkpoint_error("create snapshot parent", error))?;
            }
            fs::copy(entry.path(), &target)
                .map_err(|error| checkpoint_error("copy snapshot file", error))?;
        }
    }
    Ok(())
}

fn clear_workspace_contents(workspace: &Path) -> Result<(), ToolError> {
    for entry in
        fs::read_dir(workspace).map_err(|error| checkpoint_error("read workspace", error))?
    {
        let entry = entry.map_err(|error| checkpoint_error("read workspace entry", error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.iter().any(|skip| *skip == name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)
                .map_err(|error| checkpoint_error("clear workspace dir", error))?;
        } else {
            fs::remove_file(&path)
                .map_err(|error| checkpoint_error("clear workspace file", error))?;
        }
    }
    Ok(())
}

fn checkpoint_error(action: &str, error: std::io::Error) -> ToolError {
    ToolError::ExecutionFailed {
        name: "checkpoint".to_string(),
        message: format!("{action}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn snapshot_and_restore_round_trip() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("note.txt");
        fs::write(&file, "v1").unwrap();

        let store = CheckpointStore::new(workspace.path()).unwrap();
        let id = store.snapshot("before_turn").unwrap();

        fs::write(&file, "v2").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v2");

        store.restore(&id).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn checkpoint_storage_lives_under_deep_code_dir() {
        let workspace = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(workspace.path()).unwrap();
        store.snapshot("probe").unwrap();
        assert!(workspace.path().join(".deep-code/checkpoints").is_dir());
    }

    #[test]
    fn prunes_oldest_snapshots_beyond_cap() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("f.txt"), "x").unwrap();
        let store = CheckpointStore::new(workspace.path())
            .unwrap()
            .with_max_snapshots(3);

        let mut ids = Vec::new();
        for index in 0..5 {
            ids.push(store.snapshot(&format!("s{index}")).unwrap());
            // Distinct millisecond timestamps so retention order is stable.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let kept = store.list().unwrap();
        assert_eq!(kept.len(), 3, "retention cap must prune to max_snapshots");
        assert!(kept.contains(&ids[4]), "newest snapshot survives");
        assert!(!kept.contains(&ids[0]), "oldest snapshot is pruned");
        // Pruned snapshots can no longer be restored.
        assert!(store.restore(&ids[0]).is_err());
    }
}
