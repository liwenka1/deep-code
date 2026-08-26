//! Symlink creation for tests, shared by every test module that probes the
//! symlink boundary (workspace policy, workspace tools, root grants).
//!
//! One triage policy for all callers, so "skip" can never quietly absorb a
//! broken test:
//!
//! * unix: symlinks always work, so ANY creation failure is a bug in the test
//!   itself (EEXIST from a reused name, ENOENT from a missing parent, …) and
//!   panics. The pre-shared helpers returned `is_ok()` here, which would have
//!   demoted such a bug to a silent skip of a security-boundary test.
//! * windows: only "this machine cannot make symlinks at all" skips — the
//!   runtime right Developer Mode / elevation grants is genuinely absent on
//!   plain user boxes, and a volume without reparse-point support (FAT32,
//!   exFAT, some network shares) cannot host one at any privilege level.
//!   Everything else panics like on unix. A skip prints to stderr, but
//!   libtest captures passing tests' output, so on CI the skip is INVISIBLE;
//!   that is what [`REQUIRE_SYMLINKS_ENV`] exists for — the Windows job sets
//!   it (the hosted runner is elevated and can create symlinks), turning a
//!   runner that silently lost the privilege into a red build instead of a
//!   sweep of vacuously green boundary tests. Same contract as
//!   `DEEPCODE_REQUIRE_SANDBOX` in `sandbox`. Its reach is bounded by the
//!   failure arm it lives in, so `tests::a_symlink_attempt_always_happens`
//!   guarantees at least one attempt per binary regardless of what the
//!   boundary tests do.
//! * other targets: no symlink API to test; always skip.

use std::path::Path;

/// Set in environments known to support symlink creation: a skip becomes a
/// hard failure, so "green" always means the symlink tests actually ran.
#[cfg(windows)]
const REQUIRE_SYMLINKS_ENV: &str = "DEEPCODE_REQUIRE_SYMLINKS";

/// Symlink to a DIRECTORY target. `false` = skip (announced on stderr).
pub(crate) fn symlink_dir_for_test(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        unix_must_succeed(std::os::unix::fs::symlink(target, link), target, link)
    }
    #[cfg(windows)]
    {
        windows_triage(
            std::os::windows::fs::symlink_dir(target, link),
            target,
            link,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

/// Symlink to a FILE target (existing or dangling). `false` = skip.
pub(crate) fn symlink_file_for_test(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        unix_must_succeed(std::os::unix::fs::symlink(target, link), target, link)
    }
    #[cfg(windows)]
    {
        windows_triage(
            std::os::windows::fs::symlink_file(target, link),
            target,
            link,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

/// Remove a symlink whose target is a directory. Windows stores it as a
/// directory entry (`remove_file` refuses it); unix as a file entry.
pub(crate) fn remove_symlink_dir_for_test(link: &Path) {
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
    #[cfg(not(windows))]
    std::fs::remove_file(link).unwrap();
}

#[cfg(unix)]
fn unix_must_succeed(result: std::io::Result<()>, target: &Path, link: &Path) -> bool {
    if let Err(error) = result {
        panic!(
            "symlink {} -> {} failed on unix — a test bug, not a platform gap: {error}",
            link.display(),
            target.display()
        );
    }
    true
}

#[cfg(windows)]
fn windows_triage(result: std::io::Result<()>, target: &Path, link: &Path) -> bool {
    // The environment-cannot-do-this codes, all of which mean "no symlink is
    // possible here for anyone", never "this test is wrong":
    //
    // * 1314 ERROR_PRIVILEGE_NOT_HELD — the documented "no Developer Mode /
    //   not elevated" failure for CreateSymbolicLink.
    // * 5 ERROR_ACCESS_DENIED — the same refusal as seen through a policy or
    //   a restricted token; std maps it to `PermissionDenied`, NOT to
    //   `Unsupported`, so the kind check below never covered it.
    // * 1 ERROR_INVALID_FUNCTION and 50 ERROR_NOT_SUPPORTED — the volume has
    //   no reparse points (FAT32/exFAT, some network shares). std has no
    //   mapping for either, so both arrive as `Uncategorized` and likewise
    //   escaped the kind check.
    //
    // Only 120 ERROR_CALL_NOT_IMPLEMENTED reaches `ErrorKind::Unsupported` on
    // Windows, which is why matching on the kind alone turned a developer
    // whose %TEMP% sits on exFAT into eight hard failures instead of skips.
    const ENVIRONMENT_CANNOT: [i32; 4] = [1314, 5, 1, 50];
    match result {
        Ok(()) => true,
        Err(error)
            if error
                .raw_os_error()
                .is_some_and(|code| ENVIRONMENT_CANNOT.contains(&code))
                || error.kind() == std::io::ErrorKind::Unsupported =>
        {
            assert!(
                std::env::var_os(REQUIRE_SYMLINKS_ENV).is_none(),
                "{REQUIRE_SYMLINKS_ENV} is set but this process cannot create \
                 symlinks: {error}"
            );
            eprintln!("skipping: cannot create symlinks on this platform/user ({error})");
            false
        }
        Err(error) => panic!(
            "symlink {} -> {} failed for a non-privilege reason — a test bug: {error}",
            link.display(),
            target.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`REQUIRE_SYMLINKS_ENV`] only fires inside the failure arm of
    /// `windows_triage`, so its promise — "green means the symlink tests
    /// really ran" — holds only while SOMETHING actually attempts a symlink.
    /// Every boundary test regressing to `#[cfg(unix)]` would put the Windows
    /// job back to vacuously green with the variable set and silent. This is
    /// the accounting: one unconditional attempt per test binary, so a runner
    /// that lost the privilege goes red on this test's own account.
    ///
    /// It is also the only coverage `remove_symlink_dir_for_test` has, and
    /// that helper encodes the Windows rule (`remove_dir`, not `remove_file`)
    /// that `checkpoint::clear_workspace_contents` had to learn the hard way.
    #[test]
    fn a_symlink_attempt_always_happens() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        if !symlink_dir_for_test(&target, &link) {
            return;
        }
        assert!(
            link.symlink_metadata().unwrap().file_type().is_symlink(),
            "the helper must create a real symlink"
        );
        remove_symlink_dir_for_test(&link);
        assert!(
            link.symlink_metadata().is_err(),
            "the helper must remove the link"
        );
        assert!(
            target.is_dir(),
            "removing the link must not touch its target"
        );
    }
}
