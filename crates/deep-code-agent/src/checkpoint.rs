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

    /// Restore, returning the workspace-relative paths `clear` deliberately
    /// KEPT because the snapshot has no way to put them back.
    ///
    /// Returned rather than discarded: `restore` used to answer `Ok(())` and
    /// the UI said "workspace restored" flat out, while two surprising rules
    /// could quietly leave things behind — a link this platform cannot recreate,
    /// and an entry with no snapshot representation (a FIFO, socket or device
    /// node). "Restored, except for these" is the true sentence.
    ///
    /// The [`SKIP_DIRS`] (`.git`, `node_modules`, `target`, `.deep-code`) are
    /// kept too, but deliberately NOT in this list: they are never snapshotted,
    /// so they are kept on *every* restore, and naming them each time would be
    /// noise rather than news. The returned list is only the entries a user
    /// would be surprised to find unrestored.
    pub fn restore(&self, id: &CheckpointId) -> Result<Vec<String>, ToolError> {
        validate_checkpoint_id(id)?;
        let source = self.storage_root.join(&id.0);
        // `symlink_metadata`, not `is_dir()`: the latter follows links, and
        // the storage root sits INSIDE the workspace where the model can write.
        // A checkpoint id pointing at a symlink would have `restore` clear the
        // workspace and then copy the link's target into it. `list()` already
        // uses the non-following `DirEntry::file_type`, so such an entry never
        // appears there — the two now agree.
        if !source
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_dir())
        {
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
        let mut kept = Vec::new();
        clear_workspace_contents(&self.workspace, &source, &mut kept).map_err(|error| {
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
        kept.sort();
        Ok(kept)
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
        // Never write THROUGH something that is not a plain file or directory,
        // standing where the snapshot has content. `clear` is supposed to have
        // removed any such entry already (see `clear_dir_contents`); this is
        // the second half of the same invariant, stated where the write
        // actually happens.
        //
        // The set is [`Entry::Symlink`] and [`Entry::Uncapturable`], asked
        // through `classify` rather than spelled again — an `is_symlink()` test
        // here was narrower than the arm it is guarding and let the whole
        // FIFO/socket/device family through. `create_dir_all` and `fs::copy`
        // follow a reparse point; `fs::copy` onto a FIFO *blocks* until a
        // reader appears and onto a socket fails, neither of which the write
        // side has any business discovering.
        if mode == CopyMode::Restore
            && let Ok(meta) = target.symlink_metadata()
            && matches!(
                classify(&meta.file_type()),
                Entry::Symlink | Entry::Uncapturable
            )
        {
            return Err(checkpoint_error(
                "restore",
                std::io::Error::other(format!(
                    "{} is not a plain file or directory, and the snapshot has \
                     content for that path; refusing to write through it. \
                     Remove it and re-run the restore.",
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
            // Nothing recorded here, so nothing may be deleted either:
            // `clear_dir_contents` keeps both of these under the same rule —
            // removable only where the snapshot covers the path, because only
            // then is there something to put back.
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
/// account lacks.
///
/// Both halves of `restore` gate on THIS, rather than each spelling
/// `#[cfg(unix)]` for itself, so that hard-coding it and running the suite
/// really does exercise the other branch on both sides. `copy_tree`'s arm
/// still needs a `#[cfg(unix)]` body for the API that does not exist
/// elsewhere; see the compile-time assertion below for why that is not a
/// second, driftable condition.
///
/// [`clear_workspace_contents`] keys the delete side off this same answer.
/// Snapshot and clear must agree on exactly one set of entries, or `restore`
/// destroys something it cannot put back.
const fn snapshot_can_capture_symlink() -> bool {
    cfg!(unix)
}

/// The constant is the ONE switch both sides read, but `copy_tree`'s symlink
/// arm still has a `#[cfg(unix)]` BODY — `std::os::unix::fs::symlink` does not
/// exist elsewhere. That is a second condition, and the two agree today only
/// because the constant is itself `cfg!(unix)`.
///
/// Flipping it true on a non-unix target would make the arm match while its
/// body compiles away: `copy_tree` would record nothing while
/// `clear_dir_contents` started deleting every workspace symlink — precisely
/// the "deletes more than it stores" disagreement whose last instance was a P0.
/// So the pairing is asserted at compile time rather than described in a
/// comment: a Windows symlink capability has to bring a Windows body with it.
const _: () = assert!(
    cfg!(unix) || !snapshot_can_capture_symlink(),
    "snapshot_can_capture_symlink() is true on a target where copy_tree's \
     symlink arm compiles to nothing; give it a body for this platform first"
);

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
fn clear_workspace_contents(
    workspace: &Path,
    snapshot: &Path,
    report: &mut Vec<String>,
) -> Result<(), ToolError> {
    clear_dir_contents(workspace, snapshot, workspace, report).map(|_| ())
}

/// Returns whether anything under `dir` was deliberately KEPT, which is what
/// tells the caller not to remove `dir` itself: a directory holding a skipped
/// `.git` must survive to hold it.
fn clear_dir_contents(
    workspace: &Path,
    snapshot: &Path,
    dir: &Path,
    report: &mut Vec<String>,
) -> Result<bool, ToolError> {
    let mut kept = false;
    for entry in fs::read_dir(dir).map_err(|error| checkpoint_error("read workspace", error))? {
        let entry = entry.map_err(|error| checkpoint_error("read workspace entry", error))?;
        let path = entry.path();
        // Judged on the path relative to the workspace root, exactly as
        // `copy_tree` judges it — not on the bare entry name.
        //
        // Keeping is the fail-safe answer when the path cannot be judged at
        // all. The old `unwrap_or(&path)` fell back to the ABSOLUTE path, and
        // `Path::join` with an absolute path discards the base — so
        // `snapshot.join(relative)` became the workspace entry itself, whose
        // metadata always resolves, and "does the snapshot cover this?"
        // answered yes for everything. Unreachable today, but the direction it
        // failed in was "delete".
        let Ok(relative) = path.strip_prefix(workspace) else {
            kept = true;
            report.push(path.display().to_string());
            continue;
        };
        if should_skip(relative) {
            // Kept, but NOT pushed to `report`: SKIP_DIRS are never snapshotted,
            // so they are kept on every restore — reporting them each time is
            // noise. `restore`'s list is only the entries a user would be
            // surprised to find unrestored (see its doc).
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
                if snapshot_can_capture_symlink() || snapshot_covers(snapshot, relative) {
                    remove_symlink(&path, &file_type)
                        .map_err(|error| checkpoint_error("clear workspace symlink", error))?;
                } else {
                    kept = true;
                    report.push(relative.display().to_string());
                }
            }
            Entry::Dir => {
                // Recurse rather than `remove_dir_all`: the subtree may hold a
                // skipped directory, and blowing it away is bug 1 above.
                if clear_dir_contents(workspace, snapshot, &path, report)? {
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
            // Exactly the link arm's rule, one file-type family over — which
            // is the half that was missing. Nothing here was recorded, so this
            // entry may only be removed when the snapshot holds something to
            // put back at this very path; otherwise there is no way to restore
            // it and it stays.
            //
            // Keeping it UNCONDITIONALLY was not the safe choice it looks
            // like. A regular file captured by the snapshot, replaced in the
            // workspace by a socket or FIFO before the restore, was then kept
            // by `clear` and written *through* by `copy_tree`: onto a socket
            // that fails every time (so the workspace ends up cleared, nothing
            // restored, and the "re-run to finish it" advice is a lie), onto a
            // FIFO with no reader it blocks forever and hangs the whole app,
            // and onto one with a reader it injects the snapshot's bytes into
            // a live IPC channel and reports success.
            Entry::Uncapturable => {
                if snapshot_covers(snapshot, relative) {
                    fs::remove_file(&path)
                        .map_err(|error| checkpoint_error("clear workspace special file", error))?;
                } else {
                    kept = true;
                    report.push(relative.display().to_string());
                }
            }
        }
    }
    Ok(kept)
}

/// Does the snapshot hold an entry at this workspace-relative path?
///
/// The one question both "may `clear` delete this?" arms ask. `symlink_metadata`
/// so that a link recorded IN the snapshot still counts as coverage — the
/// question is whether `copy_tree` will write something here, not what.
fn snapshot_covers(snapshot: &Path, relative: &Path) -> bool {
    snapshot.join(relative).symlink_metadata().is_ok()
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
mod tests;
