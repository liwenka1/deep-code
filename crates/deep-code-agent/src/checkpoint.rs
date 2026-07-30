use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::tool::ToolError;

const CHECKPOINT_DIR: &str = ".deep-code/checkpoints";
const SKIP_DIRS: &[&str] = &[".git", ".deep-code", "target", "node_modules"];
/// Default retention: snapshots beyond this count are pruned oldest-first.
/// Every turn creates one before-turn snapshot, so without a cap the storage
/// grows by one workspace copy per turn. On CoW filesystems (APFS, btrfs/XFS)
/// snapshots clone rather than rewrite, so unchanged files cost no extra disk
/// and the cap mostly bounds directory count, not bytes.
pub const DEFAULT_MAX_SNAPSHOTS: usize = 20;

/// Identifier for a workspace snapshot stored outside `.git`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckpointId(pub String);

/// Side-storage workspace snapshots (taken before each turn).
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    workspace: PathBuf,
    storage_root: PathBuf,
    max_snapshots: usize,
}

impl CheckpointStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let workspace = workspace.into();
        let canonical = workspace.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                "checkpoint",
                format!(
                    "failed to resolve workspace root {}: {error}",
                    workspace.display()
                ),
            )
        })?;
        let storage_root = canonical.join(CHECKPOINT_DIR);
        fs::create_dir_all(&storage_root).map_err(|error| {
            ToolError::exec_failed(
                "checkpoint",
                format!(
                    "failed to create checkpoint storage {}: {error}",
                    storage_root.display()
                ),
            )
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
    /// oldest snapshots beyond the retention cap. Returns the id plus any
    /// non-fatal prune warnings for the caller to surface.
    pub fn snapshot(&self, label: &str) -> Result<(CheckpointId, Vec<String>), ToolError> {
        let id = format!(
            "{}_{}",
            sanitize_label(label),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis())
        );
        // Copy into a staging directory and publish with a rename, so a failure
        // part-way through can never leave a *listable* checkpoint: `list` and
        // `restore` both reject names containing `.`, and `restore` clears the
        // workspace before copying, so restoring a half-copied snapshot would
        // destroy the working tree and put back only some of it.
        //
        // A crash between the two steps leaks a `.staging_*` directory. That is
        // invisible to `list`/`restore` and costs only disk; it is deliberately
        // not swept here, because another process in the same workspace could be
        // mid-snapshot and its staging dir is none of our business.
        let dest = self.storage_root.join(&id);
        let staging = self.storage_root.join(format!(".staging_{id}"));
        if let Err(error) = copy_tree(&self.workspace, &staging, true) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging, &dest) {
            let _ = fs::remove_dir_all(&staging);
            return Err(checkpoint_error("publish snapshot", error));
        }
        let prune_warnings = self.prune_old_snapshots();
        Ok((CheckpointId(id), prune_warnings))
    }

    /// Best-effort retention: delete the oldest snapshot directories (by the
    /// trailing millisecond timestamp in their names) beyond `max_snapshots`.
    /// Failures are returned as warnings, never propagated — a full disk must
    /// not fail the snapshot that just succeeded.
    fn prune_old_snapshots(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.max_snapshots == 0 {
            return warnings;
        }
        let Ok(mut ids) = self.list() else {
            return warnings;
        };
        if ids.len() <= self.max_snapshots {
            return warnings;
        }
        ids.sort_by_key(|id| snapshot_timestamp(&id.0));
        let excess = ids.len() - self.max_snapshots;
        for id in ids.into_iter().take(excess) {
            let path = self.storage_root.join(&id.0);
            if let Err(error) = fs::remove_dir_all(&path) {
                warnings.push(format!("checkpoint prune failed for {}: {error}", id.0));
            }
        }
        warnings
    }

    pub fn restore(&self, id: &CheckpointId) -> Result<(), ToolError> {
        validate_checkpoint_id(id)?;
        let source = self.storage_root.join(&id.0);
        if !source.is_dir() {
            return Err(ToolError::exec_failed(
                "checkpoint",
                format!("checkpoint '{}' does not exist", id.0),
            ));
        }
        // Clear-then-copy is not atomic: a failure in between (a Windows file
        // lock on an open binary, a full disk) leaves the workspace part-cleared
        // and part-restored. The snapshot itself is untouched and still valid, so
        // re-running the same restore is the recovery — say so, because the raw
        // io error reads like the checkpoint was lost.
        clear_workspace_contents(&self.workspace)?;
        copy_tree(&source, &self.workspace, false).map_err(|error| {
            ToolError::exec_failed(
                "checkpoint",
                format!(
                    "{error}; the workspace is partially restored. Snapshot '{}' is intact — \
                     re-run the restore to finish it.",
                    id.0
                ),
            )
        })?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<CheckpointId>, ToolError> {
        let mut ids = Vec::new();
        let entries = fs::read_dir(&self.storage_root).map_err(|error| {
            ToolError::exec_failed("checkpoint", format!("failed to list checkpoints: {error}"))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ToolError::exec_failed(
                    "checkpoint",
                    format!("failed to read checkpoint entry: {error}"),
                )
            })?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let candidate = CheckpointId(entry.file_name().to_string_lossy().into_owned());
                // Only report directories that are actually restorable. This
                // filters in-progress `.staging_*` copies (see `snapshot`) and
                // any hand-dropped junk, so nothing unrestorable reaches the
                // `/checkpoints` list or the retention accounting.
                if validate_checkpoint_id(&candidate).is_ok() {
                    ids.push(candidate);
                }
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

/// Reject checkpoint ids that could escape the storage root. A valid id is a
/// `{label}_{millis}` name `snapshot()` produced — a single path component of
/// `[A-Za-z0-9_-]`. Anything with a path separator, `..`, or other characters
/// is a traversal attempt; ids can arrive from an HTTP request or a session
/// file, and `restore` clears the workspace before copying, so an unchecked id
/// pointing outside storage would be destructive.
fn validate_checkpoint_id(id: &CheckpointId) -> Result<(), ToolError> {
    let raw = id.0.as_str();
    let valid = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(ToolError::exec_failed(
            "checkpoint",
            format!("invalid checkpoint id '{raw}'"),
        ))
    }
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
        let rel = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ToolError::exec_failed("checkpoint", error.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if skip_meta && should_skip(rel) {
            continue;
        }
        let target = dest.join(rel);
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            // Preserve the link itself, not its referent: a workspace symlink
            // must survive snapshot → clear → restore instead of being
            // silently dropped (Unix only; Windows symlinks need privileges
            // and keep the old skip behavior).
            #[cfg(unix)]
            {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| checkpoint_error("create snapshot parent", error))?;
                }
                let link_target = fs::read_link(entry.path())
                    .map_err(|error| checkpoint_error("read snapshot symlink", error))?;
                std::os::unix::fs::symlink(&link_target, &target)
                    .map_err(|error| checkpoint_error("copy snapshot symlink", error))?;
            }
        } else if file_type.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| checkpoint_error("create snapshot subdir", error))?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| checkpoint_error("create snapshot parent", error))?;
            }
            copy_file_retrying(entry.path(), &target)
                .map_err(|error| checkpoint_error("copy snapshot file", error))?;
        }
    }
    Ok(())
}

/// Copy a file, preferring a copy-on-write clone where the filesystem supports
/// it (APFS on macOS, btrfs/XFS reflink on Linux). A cloned snapshot shares
/// extents with the live file, so per turn an unchanged file costs no data I/O
/// and ~zero extra disk across the whole retention window — cloning is purely
/// an optimization, and any clone failure (non-CoW filesystem, cross-device)
/// falls through to the plain copy below.
///
/// The plain path retries briefly on a transient Windows lock. A freshly-
/// written workspace file is often held for a few milliseconds by antivirus
/// real-time scanning or a concurrent writer; on Windows that blocks even a
/// read-copy and surfaces as `ERROR_SHARING_VIOLATION` (32) /
/// `ERROR_LOCK_VIOLATION` (33). Unix has no such lock so those codes never
/// occur there. The lock clears fast, so a short backoff turns a flaky
/// snapshot failure into a reliable one.
fn copy_file_retrying(source: &Path, dest: &Path) -> std::io::Result<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if cow::clone_file(source, dest) {
        // Match fs::copy's contract (bytes of content copied); a clone shares
        // rather than rewrites them, but the length is the same.
        return source.metadata().map(|meta| meta.len());
    }
    const BACKOFF_MS: &[u64] = &[10, 30, 100, 300];
    let mut attempt = 0;
    loop {
        match fs::copy(source, dest) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                let transient = matches!(error.raw_os_error(), Some(32) | Some(33));
                match BACKOFF_MS.get(attempt) {
                    Some(&ms) if transient => {
                        std::thread::sleep(Duration::from_millis(ms));
                        attempt += 1;
                    }
                    _ => return Err(error),
                }
            }
        }
    }
}

/// Best-effort copy-on-write file clones. `clone_file` returns `false` for any
/// failure — the caller falls back to a plain copy, so nothing here may leave a
/// half-written destination behind.
#[cfg(target_os = "macos")]
mod cow {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// APFS `clonefile(2)`: clones content and metadata (mode included) in one
    /// call. Flags stay 0 — callers only pass regular files (symlinks take the
    /// dedicated branch in `copy_tree`) and always create fresh destinations,
    /// so there is nothing to not-follow and no existing dest to contend with.
    ///
    /// A failed `clonefile` leaves no partial destination of its own (it either
    /// creates the clone or nothing), and the caller's `fs::copy` fallback
    /// truncates whatever is there — so unlike the Linux path this needs no
    /// explicit cleanup.
    pub(super) fn clone_file(source: &Path, dest: &Path) -> bool {
        let (Ok(src), Ok(dst)) = (
            CString::new(source.as_os_str().as_bytes()),
            CString::new(dest.as_os_str().as_bytes()),
        ) else {
            return false; // interior NUL — let fs::copy report it properly
        };
        unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) == 0 }
    }
}

#[cfg(target_os = "linux")]
mod cow {
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    /// `FICLONE` = `_IOW(0x94, 9, int)`, spelled out so this does not depend on
    /// the libc crate exporting it.
    ///
    /// This is the *asm-generic* ioctl encoding — correct on x86_64, aarch64,
    /// arm, riscv and s390x, i.e. every target this project ships. It is not
    /// universal: powerpc/mips/sparc encode `_IOW` differently (`0x8004_9409`),
    /// where this value decodes as a different direction, the ioctl returns
    /// `ENOTTY`, and the caller silently falls back to `fs::copy`. Harmless, but
    /// it means reflink is simply unavailable there rather than misbehaving.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    /// btrfs/XFS reflink via the `FICLONE` ioctl. Unlike macOS `clonefile`,
    /// this clones content only, so the source's permission bits are applied
    /// afterwards (fs::copy preserves them; the clone path must too — a
    /// restored script keeps its executable bit).
    pub(super) fn clone_file(source: &Path, dest: &Path) -> bool {
        let Ok(src) = File::open(source) else {
            return false;
        };
        let Ok(dst) = File::create(dest) else {
            return false;
        };
        let cloned = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0;
        drop(dst);
        if !cloned {
            // ext4 etc.: remove the empty dest so the fs::copy fallback starts
            // from the same fresh-target state every other path sees.
            let _ = std::fs::remove_file(dest);
            return false;
        }
        if let Ok(meta) = source.metadata() {
            let _ = std::fs::set_permissions(dest, meta.permissions());
        }
        true
    }
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
        // `file_type()` (unlike `path.is_dir()`) does not follow symlinks: a
        // link to a directory is removed as the link it is, never traversed
        // into its (possibly workspace-external) referent.
        let file_type = entry
            .file_type()
            .map_err(|error| checkpoint_error("stat workspace entry", error))?;
        if file_type.is_dir() {
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
    ToolError::exec_failed("checkpoint", format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn copy_file_retrying_copies_contents() {
        // On macOS/Linux this exercises the CoW clone fast path (falling back
        // to fs::copy on non-CoW filesystems); the contract is identical
        // either way: same contents, fs::copy's byte count.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, "payload").unwrap();
        let bytes = copy_file_retrying(&src, &dst).unwrap();
        assert_eq!(bytes, "payload".len() as u64);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    }

    /// Whichever path runs (clone or plain copy), the destination must keep
    /// the source's permission bits — a restored script loses nothing.
    #[cfg(unix)]
    #[test]
    fn copy_preserves_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("run.sh");
        let dst = dir.path().join("copy.sh");
        fs::write(&src, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        copy_file_retrying(&src, &dst).unwrap();
        let mode = fs::metadata(&dst).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "executable bit must survive the copy");
    }

    /// A directory that is not a publishable snapshot must be invisible to both
    /// `list` and `restore`. That is what keeps a crashed snapshot (which leaves
    /// a `.staging_*` tree behind) from being offered as a rollback target and
    /// then restored over a cleared workspace.
    #[test]
    fn staging_and_junk_directories_are_neither_listed_nor_restorable() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("note.txt"), "v1").unwrap();
        let store = CheckpointStore::new(workspace.path()).unwrap();
        let storage = store.storage_root.clone();

        let (good, _) = store.snapshot("before_turn").unwrap();

        // Simulate the crash residue and some hand-dropped junk.
        fs::create_dir_all(storage.join(".staging_before_turn_123")).unwrap();
        fs::create_dir_all(storage.join("has.dot")).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(
            listed,
            vec![good.clone()],
            "only publishable snapshots are listed"
        );
        assert!(
            store
                .restore(&CheckpointId(".staging_before_turn_123".to_string()))
                .is_err(),
            "a staging tree must never be restorable"
        );
        // The workspace was not touched by the rejected restore.
        assert_eq!(
            fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "v1"
        );
    }

    #[test]
    fn snapshot_publishes_atomically() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("note.txt"), "v1").unwrap();
        let store = CheckpointStore::new(workspace.path()).unwrap();
        let storage = store.storage_root.clone();

        let (id, _) = store.snapshot("before_turn").unwrap();

        // Published under its real id, with no staging residue left behind.
        assert!(storage.join(&id.0).is_dir());
        let leftovers: Vec<_> = fs::read_dir(&storage)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".staging_"))
            .collect();
        assert!(leftovers.is_empty(), "staging residue: {leftovers:?}");
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("note.txt");
        fs::write(&file, "v1").unwrap();

        let store = CheckpointStore::new(workspace.path()).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();

        fs::write(&file, "v2").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v2");

        store.restore(&id).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }

    /// A workspace symlink must survive snapshot → clear → restore as a link
    /// (same target), not be silently dropped or traversed into.
    #[cfg(unix)]
    #[test]
    fn restore_preserves_symlinks() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("shared.txt"), "external").unwrap();
        fs::write(workspace.path().join("real.txt"), "v1").unwrap();
        let link = workspace.path().join("link-out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let store = CheckpointStore::new(workspace.path()).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();

        fs::remove_file(&link).unwrap();
        store.restore(&id).unwrap();

        assert_eq!(
            fs::read_link(&link).unwrap(),
            outside.path(),
            "restore must recreate the symlink with its original target"
        );
        // The link's external referent was never cleared or copied into.
        assert_eq!(
            fs::read_to_string(outside.path().join("shared.txt")).unwrap(),
            "external"
        );
    }

    /// The `clear` half of restore must delete a workspace symlink as the link
    /// it is, never recurse through it into a workspace-external directory. The
    /// link is created *after* the snapshot so it is still present when restore
    /// clears the workspace (this is the path `restore_preserves_symlinks`
    /// doesn't reach — it removes the link before restoring).
    #[cfg(unix)]
    #[test]
    fn restore_clears_symlink_without_deleting_external_target() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("shared.txt"), "external").unwrap();
        fs::write(workspace.path().join("real.txt"), "v1").unwrap();

        let store = CheckpointStore::new(workspace.path()).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();

        // Introduce the external link only now, so it is live in the workspace
        // when `restore` clears it.
        let link = workspace.path().join("link-out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        store.restore(&id).unwrap();

        assert!(
            !link.exists() && link.symlink_metadata().is_err(),
            "clear must remove the stray symlink"
        );
        assert!(
            outside.path().join("shared.txt").exists(),
            "clear must not follow the link and delete its external target"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("shared.txt")).unwrap(),
            "external"
        );
    }

    #[test]
    fn restore_rejects_traversal_ids_without_touching_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("keep.txt");
        fs::write(&file, "live").unwrap();
        let store = CheckpointStore::new(workspace.path()).unwrap();

        for bad in ["../escape", "..", "a/b", "a\\b", "", "with space"] {
            let err = store.restore(&CheckpointId(bad.to_string())).unwrap_err();
            assert!(
                format!("{err:?}").contains("invalid checkpoint id"),
                "id {bad:?} must be rejected before any workspace mutation"
            );
        }
        // The workspace was never cleared by a rejected restore.
        assert_eq!(fs::read_to_string(&file).unwrap(), "live");
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
            ids.push(store.snapshot(&format!("s{index}")).unwrap().0);
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
