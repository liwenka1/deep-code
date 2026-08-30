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
    let kept = store.restore(&id).unwrap();

    // ...and `restore` SAYS so. It used to answer a flat `Ok(())` while
    // three separate rules could leave things behind, and the UI printed
    // "workspace restored" on top of that.
    assert_eq!(
        kept,
        vec!["dev.sock".to_string()],
        "the entry that could not be restored must be reported, not implied"
    );
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

/// `copy_tree`'s own guard, driven directly.
///
/// It has to be driven directly to be tested at all: whenever `clear` does
/// its job the guard is unreachable through `restore`, so before this test
/// the whole block could be deleted and every checkpoint test stayed green
/// on both branches of `snapshot_can_capture_symlink`. That is the state
/// this file's own comment calls "the second half of the same invariant" —
/// a second half nothing was holding. It earns its place against a
/// concurrent writer planting an entry between the clear and the copy.
///
/// Both members of the refusal set are exercised, because an `is_symlink()`
/// spelling here (what it used to be) passes the first and fails the second.
#[cfg(unix)]
#[test]
fn copy_tree_refuses_to_write_through_anything_that_is_not_a_plain_entry() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("untouched.txt"), "VICTIM").unwrap();
    fs::create_dir_all(source.path().join("d")).unwrap();
    fs::write(source.path().join("d/f.txt"), "snapshot-content").unwrap();

    // Asserting `is_err()` alone would be a false green for the socket:
    // with a narrower guard the copy reaches `fs::copy`, fails on its own
    // with EOPNOTSUPP, and returns Err anyway — the right answer for the
    // wrong reason, and only after the write side has already blocked on
    // the retry backoff. So both cases assert the REFUSAL, by its wording.
    const REFUSAL: &str = "refusing to write through it";

    // A directory symlink pointing out of the workspace.
    let linked = tempfile::tempdir().unwrap();
    crate::test_symlinks::symlink_dir_for_test(outside.path(), &linked.path().join("d"));
    let refused = copy_tree(source.path(), linked.path(), CopyMode::Restore);
    let message = refused.expect_err("wrote through a symlink").to_string();
    assert!(message.contains(REFUSAL), "wrong cause: {message}");
    assert!(!outside.path().join("f.txt").exists());
    assert_eq!(
        fs::read_to_string(outside.path().join("untouched.txt")).unwrap(),
        "VICTIM"
    );

    // A socket standing where the source holds a regular file.
    let sock_dest = tempfile::tempdir().unwrap();
    fs::create_dir_all(sock_dest.path().join("d")).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(sock_dest.path().join("d/f.txt")).unwrap();
    drop(listener);
    let refused = copy_tree(source.path(), sock_dest.path(), CopyMode::Restore);
    let message = refused.expect_err("wrote through a socket").to_string();
    assert!(message.contains(REFUSAL), "wrong cause: {message}");
}

/// The complement of `restore_keeps_entries_it_cannot_capture`: there the
/// FIFO pre-dates the snapshot, so nothing was recorded at that path and
/// keeping it is right. Here the snapshot holds a REGULAR FILE at the path
/// and the special file appeared afterwards — so `clear` must remove it,
/// because `copy_tree` is about to put the captured file back.
///
/// Keeping it instead meant `fs::copy` ran onto a live socket: it fails
/// with the same error on every attempt (nothing ever removes the socket),
/// so the workspace stayed cleared with nothing restored while the message
/// promised that re-running would finish the job. A FIFO with no reader is
/// worse still — `fs::copy` opens it `O_WRONLY` and blocks forever, and
/// `restore_checkpoint` is called synchronously under `block_in_place`, so
/// the whole TUI hangs with no timeout. A socket is used here because it
/// fails fast rather than wedging the suite.
#[cfg(unix)]
#[test]
fn restore_removes_an_uncapturable_entry_standing_on_snapshotted_content() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    fs::write(root.join("dev.sock"), "was-a-regular-file").unwrap();
    let store = CheckpointStore::new(root).unwrap();
    let (id, _) = store.snapshot("before_turn").unwrap();

    fs::remove_file(root.join("dev.sock")).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(root.join("dev.sock")).unwrap();
    drop(listener);

    store
        .restore(&id)
        .expect("a special file the snapshot covers must be cleared, not written through");

    assert_eq!(
        fs::read_to_string(root.join("dev.sock")).unwrap(),
        "was-a-regular-file"
    );
}

/// The storage root lives inside the workspace, so the model can create
/// entries there. `restore` decided "is this a checkpoint?" with
/// `is_dir()`, which follows links — so an id naming a symlink cleared the
/// workspace and then copied the link's TARGET into it. `list()` has always
/// used the non-following `DirEntry::file_type`, so such an entry never
/// showed up there; the two disagreed.
#[test]
fn restore_refuses_a_checkpoint_id_that_is_a_symlink() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("planted.txt"), "FROM-OUTSIDE").unwrap();
    let root = workspace.path();
    fs::write(root.join("mine.txt"), "ORIGINAL").unwrap();
    let store = CheckpointStore::new(root).unwrap();
    let link = root.join(".deep-code/checkpoints/evil_1");
    if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &link) {
        return;
    }

    let refused = store.restore(&CheckpointId("evil_1".to_string()));

    assert!(
        refused.is_err(),
        "a symlinked checkpoint id must be refused"
    );
    assert_eq!(
        fs::read_to_string(root.join("mine.txt")).unwrap(),
        "ORIGINAL",
        "the workspace must not have been cleared"
    );
    assert!(!root.join("planted.txt").exists());
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
