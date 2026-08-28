use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::tool::ToolError;

const CHECKPOINT_DIR: &str = ".deep-code/checkpoints";
/// `.deep-code` and `checkpoints` inside it — the levels deep-code owns.
const OWNED_STORAGE_DIRS: usize = 2;
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
        // Both levels of `.deep-code/checkpoints` are ours and must be real
        // directories: `create_dir_all` follows a symlink at either one, and
        // every snapshot of the whole workspace is written under here.
        crate::paths::ensure_owned_dirs(&storage_root, OWNED_STORAGE_DIRS).map_err(|error| {
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
        if let Err(error) = copy_tree(&self.workspace, &staging, CopyMode::Snapshot) {
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
        //
        // Both halves get that framing, not just the copy. A failure HERE is
        // the strictly worse state — files already deleted, nothing put back
        // yet — and it used to propagate raw: the user was told an unlink
        // failed and nothing told them their workspace had just been partly
        // emptied.
        clear_workspace_contents(&self.workspace, &source).map_err(|error| {
            ToolError::exec_failed(
                "checkpoint",
                format!(
                    "{}; the workspace is partially cleared and nothing has been restored \
                     yet. Snapshot '{}' is intact — re-run the restore to finish it.",
                    error.message(),
                    id.0
                ),
            )
        })?;
        copy_tree(&source, &self.workspace, CopyMode::Restore).map_err(|error| {
            ToolError::exec_failed(
                "checkpoint",
                format!(
                    "{}; the workspace is partially restored. Snapshot '{}' is intact — \
                     re-run the restore to finish it.",
                    error.message(),
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

/// What the snapshot is able to record for one directory entry.
///
/// Both halves of `restore` ask this one function, because the invariant that
/// makes `restore` safe is that they classify every entry the same way: `clear`
/// may delete only what `copy_tree` can put back.
enum Entry {
    Symlink,
    Dir,
    File,
    /// FIFO, unix socket, device node. `copy_tree` has no representation for
    /// these, so `clear_dir_contents` must not remove them either. The two
    /// chains used to disagree here by accident: the copy side ended in a
    /// guarded `else if is_file()`, the clear side in a bare `else` that
    /// unlinked whatever was left. A dev server's socket in the workspace was
    /// therefore never captured and always deleted, with `restore` reporting
    /// success — the same "deletes more than it stores" shape as the nested
    /// skip dirs, one file-type family over.
    Uncapturable,
}

fn classify(file_type: &fs::FileType) -> Entry {
    if file_type.is_symlink() {
        Entry::Symlink
    } else if file_type.is_dir() {
        Entry::Dir
    } else if file_type.is_file() {
        Entry::File
    } else {
        Entry::Uncapturable
    }
}

/// Which direction [`copy_tree`] is running in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyMode {
    /// workspace → staging. `dest` is a freshly created private directory, and
    /// [`SKIP_DIRS`] is applied.
    Snapshot,
    /// snapshot → workspace. `dest` is the live workspace, which may still hold
    /// entries `clear_workspace_contents` deliberately kept.
    Restore,
}

fn copy_tree(source: &Path, dest: &Path, mode: CopyMode) -> Result<(), ToolError> {
    fs::create_dir_all(dest).map_err(|error| checkpoint_error("create snapshot dir", error))?;
    let mut walk = WalkDir::new(source).into_iter();
    while let Some(entry) = walk.next() {
        // A walk error must never be skipped. `filter_map(Result::ok)` dropped
        // an unreadable directory *together with its entire subtree* and still
        // returned `Ok`, so `snapshot` renamed a silently partial tree into
        // place as a valid restore point. `restore` clears the workspace before
        // copying, so restoring such a snapshot deleted the very files it had
        // never captured — unrecoverable. Failing here instead leaves only an
        // orphaned `.staging_*` dir, which `list`/`restore` already ignore.
        let entry =
            entry.map_err(|error| checkpoint_error("walk snapshot source", error.into()))?;
        let rel = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ToolError::exec_failed("checkpoint", error.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if mode == CopyMode::Snapshot && should_skip(rel) {
            // Prune, don't just `continue`: a bare `continue` still descends.
            // Every before-turn snapshot therefore walked all of `node_modules`,
            // `target` and `.git` — and, because snapshots live at
            // `.deep-code/checkpoints` INSIDE the workspace, all of the
            // retained snapshots too, so the per-turn stat count grew with the
            // retention window. An unreadable directory anywhere in there also
            // failed the whole snapshot (the walk error above is deliberately
            // fatal), every turn, over content the snapshot did not even want.
            if entry.file_type().is_dir() {
                walk.skip_current_dir();
            }
            continue;
        }
        let target = dest.join(rel);
        let file_type = entry.file_type();
        // Never write THROUGH a link standing where the snapshot has content.
        // `clear` is supposed to have removed any such link already (see
        // `clear_dir_contents`); this is the second half of the same invariant,
        // stated where the write actually happens. `create_dir_all` and
        // `fs::copy` both follow a reparse point, so without it a junction left
        // in place turned `restore` into a write outside the workspace.
        if mode == CopyMode::Restore
            && target
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(checkpoint_error(
                "restore",
                std::io::Error::other(format!(
                    "{} is a symlink the snapshot could not capture; refusing to \
                     write through it. Remove it and re-run the restore.",
                    target.display()
                )),
            ));
        }
        match classify(&file_type) {
            // Preserve the link itself, not its referent: a workspace symlink
            // must survive snapshot → clear → restore instead of being
            // silently dropped.
            //
            // Gated on the same [`snapshot_can_capture_symlink`] the delete
            // side reads, rather than on a second `#[cfg(unix)]` spelling of
            // it. Two independent spellings of one rule cannot be made to fail
            // together, and this pair is exactly the one whose disagreement
            // turned a loud aborted restore into a silent permanent loss.
            Entry::Symlink if snapshot_can_capture_symlink() => {
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
            }
            // Nothing recorded, so nothing may be deleted: `clear_dir_contents`
            // keeps both of these for the same reason.
            Entry::Symlink | Entry::Uncapturable => {}
            Entry::Dir => {
                fs::create_dir_all(&target)
                    .map_err(|error| checkpoint_error("create snapshot subdir", error))?;
            }
            Entry::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| checkpoint_error("create snapshot parent", error))?;
                }
                copy_file_retrying(entry.path(), &target)
                    .map_err(|error| checkpoint_error("copy snapshot file", error))?;
            }
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

/// Whether [`copy_tree`] records symlinks on this platform.
///
/// Unix: yes, as the link itself. Windows: no — a junction has no
/// privilege-free creation API in `std` at all, and a directory symlink needs
/// `SeCreateSymbolicLinkPrivilege` (or Developer Mode), which an ordinary
/// account lacks, so `copy_tree`'s symlink arm is `#[cfg(unix)]`.
///
/// [`clear_workspace_contents`] keys the delete side off this same answer.
/// Snapshot and clear must agree on exactly one set of entries, or `restore`
/// destroys something it cannot put back.
const fn snapshot_can_capture_symlink() -> bool {
    cfg!(unix)
}

/// Empty the workspace of everything [`copy_tree`] put into the snapshot — and
/// of nothing else.
///
/// Symmetry with the snapshot side is the entire contract here: this may
/// delete only what `restore` is able to write back. It used to break that
/// twice, in both directions.
///
/// 1. `should_skip` excludes [`SKIP_DIRS`] at **any** depth, while this loop
///    compared only the top-level entry name. So `sub/.git` never entered the
///    snapshot and `fs::remove_dir_all(sub)` deleted it regardless — a
///    vendored git clone (normally gitignored, so `git` itself cannot recover
///    it) lost its whole history, and `restore` still reported success. The
///    walk below asks `should_skip` about the same relative path the snapshot
///    side judges, so the two sets are the same set by construction.
/// 2. Windows symlinks: see the `Entry::Symlink` arm.
/// 3. Entries with no snapshot representation at all: see [`Entry::Uncapturable`].
fn clear_workspace_contents(workspace: &Path, snapshot: &Path) -> Result<(), ToolError> {
    clear_dir_contents(workspace, snapshot, workspace).map(|_| ())
}

/// Returns whether anything under `dir` was deliberately KEPT, which is what
/// tells the caller not to remove `dir` itself: a directory holding a skipped
/// `.git` must survive to hold it.
fn clear_dir_contents(workspace: &Path, snapshot: &Path, dir: &Path) -> Result<bool, ToolError> {
    let mut kept = false;
    for entry in fs::read_dir(dir).map_err(|error| checkpoint_error("read workspace", error))? {
        let entry = entry.map_err(|error| checkpoint_error("read workspace entry", error))?;
        let path = entry.path();
        // Judged on the path relative to the workspace root, exactly as
        // `copy_tree` judges it — not on the bare entry name.
        let relative = path.strip_prefix(workspace).unwrap_or(&path);
        if should_skip(relative) {
            kept = true;
            continue;
        }
        // `file_type()` (unlike `path.is_dir()`) does not follow symlinks: a
        // link to a directory is removed as the link it is, never traversed
        // into its (possibly workspace-external) referent.
        let file_type = entry
            .file_type()
            .map_err(|error| checkpoint_error("stat workspace entry", error))?;
        match classify(&file_type) {
            Entry::Symlink => {
                // Deletable when `restore` can put *something right* back at
                // this path — which is true in two separate cases:
                //
                // * the platform records links, so the snapshot holds this one
                //   and will recreate it; or
                // * the snapshot holds an entry here anyway. The link post-dates
                //   the snapshot, so it is not part of the state being restored,
                //   and `copy_tree` is about to write the captured directory or
                //   file over this path. Windows CAN delete a junction
                //   (`remove_symlink` → `remove_dir`); what it cannot do is
                //   *recreate* one, and recreating is not required here.
                //
                // Keeping a link that stands where the snapshot has content is
                // what turned `restore` into a write outside the workspace:
                // `create_dir_all`/`fs::copy` follow a reparse point, and a
                // junction needs no privilege to create. Only a link the
                // snapshot neither holds nor can recreate is kept.
                if snapshot_can_capture_symlink()
                    || snapshot.join(relative).symlink_metadata().is_ok()
                {
                    remove_symlink(&path, &file_type)
                        .map_err(|error| checkpoint_error("clear workspace symlink", error))?;
                } else {
                    kept = true;
                }
            }
            Entry::Dir => {
                // Recurse rather than `remove_dir_all`: the subtree may hold a
                // skipped directory, and blowing it away is bug 1 above.
                if clear_dir_contents(workspace, snapshot, &path)? {
                    kept = true;
                } else {
                    fs::remove_dir(&path)
                        .map_err(|error| checkpoint_error("clear workspace dir", error))?;
                }
            }
            Entry::File => {
                fs::remove_file(&path)
                    .map_err(|error| checkpoint_error("clear workspace file", error))?;
            }
            // No snapshot representation, so no way to put it back.
            Entry::Uncapturable => kept = true,
        }
    }
    Ok(kept)
}

/// Delete a symlink as the LINK, on either platform.
///
/// Unix records every symlink as a file entry, so `remove_file` is the whole
/// story. Windows records a symlink to a directory — and a junction, whose
/// reparse tag sets the name-surrogate bit — as a DIRECTORY entry:
/// `is_dir()` still reports false (it is `!is_symlink && is_directory`), so
/// the old single `remove_file` branch reached `DeleteFileW`, which refuses a
/// directory entry with access denied, and the whole `restore` aborted. A
/// junction needs no privilege to create (`mklink /J`), so any Windows
/// workspace holding one could not be restored at all.
///
/// `crate::test_symlinks::remove_symlink_dir_for_test` encodes the same rule
/// for tests; this is the production half it was missing.
fn remove_symlink(path: &Path, file_type: &fs::FileType) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if file_type.is_symlink_dir() {
            return fs::remove_dir(path);
        }
    }
    #[cfg(not(windows))]
    let _ = file_type;
    fs::remove_file(path)
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

    /// An unreadable directory must fail the snapshot, not silently vanish from
    /// it. Before this, `filter_map(Result::ok)` dropped the directory *and its
    /// whole subtree* and still returned `Ok`, so a partial tree was published
    /// as a valid restore point — and since `restore` clears the workspace
    /// first, restoring it deleted the files it had never captured.
    #[cfg(unix)]
    #[test]
    fn unreadable_directory_fails_the_snapshot_instead_of_being_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("keep.txt"), "v1").unwrap();
        let secret = workspace.path().join("locked");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("inner.txt"), "hidden").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

        let store = CheckpointStore::new(workspace.path()).unwrap();
        let result = store.snapshot("before_turn");

        // Restore permissions first so tempdir cleanup can succeed regardless.
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o755)).unwrap();

        // Running as root ignores the permission bits entirely; only assert the
        // real behavior when the directory is genuinely unreadable.
        if fs::read_dir(workspace.path().join("locked")).is_ok() && result.is_ok() {
            let ran_as_root = unsafe { libc::geteuid() } == 0;
            assert!(ran_as_root, "unreadable dir was silently skipped");
            return;
        }
        let error = result.expect_err("snapshot must fail on an unreadable subtree");
        assert!(
            error.to_string().contains("walk snapshot source"),
            "unexpected error: {error}"
        );
        // Nothing publishable may be left behind.
        assert!(store.list().unwrap().is_empty());
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

    /// `should_skip` excludes `SKIP_DIRS` at any depth, so a NESTED `.git` is
    /// never captured. The delete side used to match only top-level names and
    /// reach it through `remove_dir_all` on the parent — deleting, with no
    /// warning and an `Ok` from `restore`, the entire history of a vendored
    /// clone that is normally gitignored and therefore unrecoverable.
    ///
    /// The parent directory has to survive too: it is only there to hold the
    /// thing being kept.
    #[test]
    fn restore_keeps_nested_skip_dirs_it_never_snapshotted() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let nested = root.join("vendor/lib");
        fs::create_dir_all(nested.join(".git")).unwrap();
        fs::create_dir_all(nested.join("node_modules")).unwrap();
        fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::write(nested.join("node_modules/pkg.js"), "module.exports={}").unwrap();
        fs::write(nested.join("main.rs"), "v1").unwrap();

        let store = CheckpointStore::new(root).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();
        fs::write(nested.join("main.rs"), "v2").unwrap();
        store.restore(&id).unwrap();

        // The tracked file round-trips, as always.
        assert_eq!(fs::read_to_string(nested.join("main.rs")).unwrap(), "v1");
        // And the untracked-by-design ones are still there. Existence is
        // asserted before contents so a deletion reports itself by name
        // instead of panicking inside `read_to_string`.
        assert!(
            nested.join(".git/HEAD").is_file(),
            "restore deleted a nested .git the snapshot never captured"
        );
        assert_eq!(
            fs::read_to_string(nested.join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main"
        );
        assert!(
            nested.join("node_modules/pkg.js").is_file(),
            "restore deleted nested node_modules the snapshot never captured"
        );
    }

    /// A workspace symlink must survive snapshot → clear → restore as a link
    /// (same target), not be silently dropped or traversed into.
    #[cfg(unix)]
    #[test]
    fn restore_preserves_symlinks() {
        // Asserts a *capability*, so it is gated on the capability and not on
        // `cfg(unix)` alone. That also keeps the "hard-code the constant and
        // run the whole suite" check honest: now that one constant switches
        // both halves, forcing it off must not leave a test demanding the
        // behaviour the constant just withdrew.
        if !snapshot_can_capture_symlink() {
            return;
        }
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
    ///
    /// Runs on Windows too, and that is the point: `clear_workspace_contents`
    /// is not cfg-gated, and a DIRECTORY symlink is the one case whose Windows
    /// spelling genuinely differs (`remove_file` refuses it). Leaving this
    /// `#[cfg(unix)]` is how that platform-specific abort stayed invisible
    /// through the cross-platform pass in 93c4280/d84b22c.
    ///
    /// The link's FATE is platform-split (see `snapshot_can_capture_symlink`);
    /// the external target surviving is not, and that is the invariant this
    /// test is really named after.
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
        if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &link) {
            return;
        }

        store.restore(&id).unwrap();

        // Clear may delete only what the snapshot could capture. On unix the
        // link is recreatable, so it goes and the workspace really returns to
        // the snapshot. On Windows it is not — a junction has no
        // privilege-free creation API in `std` — so deleting it would destroy
        // something `restore` cannot put back, and it is kept instead.
        if snapshot_can_capture_symlink() {
            assert!(
                !link.exists() && link.symlink_metadata().is_err(),
                "clear must remove a stray symlink the snapshot can recreate"
            );
        } else {
            assert!(
                link.symlink_metadata().is_ok(),
                "a link the snapshot cannot capture must be kept, not destroyed"
            );
        }
        // Platform-independent, and the real point of this test: whichever
        // branch ran, clear must never recurse THROUGH the link into a
        // workspace-external directory.
        assert!(
            outside.path().join("shared.txt").exists(),
            "clear must not follow the link and delete its external target"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("shared.txt")).unwrap(),
            "external"
        );
    }

    /// The complement the test above does not reach: a link standing where the
    /// snapshot DOES hold an entry.
    ///
    /// `restore` is clear-then-copy, and `copy_tree` writes with
    /// `create_dir_all`/`fs::copy`, both of which follow a reparse point. So a
    /// link kept by the clear half at a path the copy half is about to write
    /// sent the snapshot's contents outside the workspace, with `restore`
    /// returning `Ok`. A junction needs no privilege on Windows, which is the
    /// platform whose keep-branch made this reachable.
    ///
    /// Both assertions are platform-independent by design: the link is deleted
    /// on either platform now (the snapshot covers the path, so `restore`
    /// rebuilds it — recreating the *link* is not required), so the workspace
    /// really returns to the snapshot and the external target is never touched.
    #[test]
    fn restore_never_writes_through_a_link_standing_on_snapshotted_content() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("untouched.txt"), "VICTIM").unwrap();
        let root = workspace.path();
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/f.txt"), "snapshot-content").unwrap();

        let store = CheckpointStore::new(root).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();

        // Swap the captured directory for a link pointing out of the workspace.
        fs::remove_dir_all(root.join("d")).unwrap();
        if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &root.join("d")) {
            return;
        }

        store.restore(&id).unwrap();

        assert!(
            !outside.path().join("f.txt").exists(),
            "restore wrote through the link into a workspace-external directory"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("untouched.txt")).unwrap(),
            "VICTIM"
        );
        assert_eq!(
            fs::read_to_string(root.join("d/f.txt")).unwrap(),
            "snapshot-content",
            "the snapshot's own content must be restored at that path"
        );
    }

    /// The recovery advice on a half-done restore is a promise to the user, and
    /// nothing asserted either half of it: both strings appeared only in the
    /// production `format!`. Pins the framing AND that re-running really does
    /// finish the job — plus the absence of the doubled
    /// `tool execution failed for checkpoint:` prefix that came from rendering
    /// one `ToolError` inside another.
    #[cfg(unix)]
    #[test]
    fn a_failed_clear_says_the_snapshot_is_intact_and_re_running_finishes_it() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), "v1").unwrap();

        let store = CheckpointStore::new(root).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();
        fs::write(root.join("sub/a.txt"), "v2").unwrap();

        // Make the clear half fail part-way: an unreadable subdirectory.
        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(root.join("sub")).is_ok() {
            fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
            return; // running as root; the refusal cannot be produced
        }
        let error = store.restore(&id).expect_err("the clear half must fail");
        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();

        let rendered = error.to_string();
        assert!(
            rendered.contains("partially cleared") && rendered.contains("re-run the restore"),
            "a half-cleared workspace must say so and name the way out: {rendered}"
        );
        assert_eq!(
            rendered
                .matches("tool execution failed for checkpoint:")
                .count(),
            1,
            "the wrapper stuttered its own prefix: {rendered}"
        );
        // And the advice is true.
        store
            .restore(&id)
            .expect("re-running must finish the restore");
        assert_eq!(fs::read_to_string(root.join("sub/a.txt")).unwrap(), "v1");
    }

    /// A FIFO has no snapshot representation, so `clear` may not remove it.
    /// The copy side ended in a guarded `else if is_file()` while the clear
    /// side ended in a bare `else`, so this was captured by neither and
    /// deleted by one — `restore` reported success and the socket was gone.
    #[cfg(unix)]
    #[test]
    fn restore_keeps_entries_it_cannot_capture() {
        use std::os::unix::ffi::OsStrExt;
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        fs::write(root.join("real.txt"), "v1").unwrap();
        let fifo = root.join("dev.sock");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path in a fresh tempdir.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);

        let store = CheckpointStore::new(root).unwrap();
        let (id, _) = store.snapshot("before_turn").unwrap();
        fs::write(root.join("real.txt"), "v2").unwrap();
        store.restore(&id).unwrap();

        assert_eq!(fs::read_to_string(root.join("real.txt")).unwrap(), "v1");
        assert!(
            fifo.symlink_metadata().is_ok(),
            "restore deleted a FIFO the snapshot never captured"
        );
    }

    /// The walk is pruned at a skipped directory, not merely `continue`d past.
    /// A bare `continue` still descends, so an unreadable directory inside
    /// `node_modules` failed the whole snapshot — every turn, over content the
    /// snapshot does not even want. (The same descent is why each before-turn
    /// snapshot also walked every retained snapshot under `.deep-code`.)
    #[cfg(unix)]
    #[test]
    fn snapshot_does_not_descend_into_skipped_directories() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        fs::write(root.join("main.rs"), "v1").unwrap();
        let locked = root.join("node_modules/.cache");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("blob"), "x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let store = CheckpointStore::new(root).unwrap();
        let taken = store.snapshot("before_turn");
        // Restore permissions before any assertion can panic and leak them.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            taken.is_ok(),
            "an unreadable directory inside a skipped tree failed the snapshot: {:?}",
            taken.err()
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

    /// Snapshots of the entire workspace are written under the storage root, so
    /// a symlinked `.deep-code` copied the whole tree outside the workspace —
    /// `create_dir_all` follows a link at any component. Same rule, same
    /// helper, as the session store and the stderr log.
    #[test]
    fn a_symlinked_state_dir_does_not_relocate_the_store() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state_dir = workspace.path().join(".deep-code");
        if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &state_dir) {
            return;
        }

        let refused = CheckpointStore::new(workspace.path());

        assert!(
            refused.is_err(),
            "a symlinked .deep-code must be refused, not followed"
        );
        assert!(
            !outside.path().join("checkpoints").exists(),
            "checkpoint storage was created outside the workspace"
        );
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
