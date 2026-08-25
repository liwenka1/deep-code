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
//! * windows: only the missing-privilege errors skip — the runtime right that
//!   Developer Mode / elevation grants is genuinely absent on plain user
//!   boxes. Everything else panics like on unix. A skip prints to stderr, but
//!   libtest captures passing tests' output, so on CI the skip is INVISIBLE;
//!   that is what [`REQUIRE_SYMLINKS_ENV`] exists for — the Windows job sets
//!   it (the hosted runner is elevated and can create symlinks), turning a
//!   runner that silently lost the privilege into a red build instead of six
//!   vacuously green boundary tests. Same contract as
//!   `DEEPCODE_REQUIRE_SANDBOX` in `sandbox`.
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
    // ERROR_PRIVILEGE_NOT_HELD: the documented "no Developer Mode / not
    // elevated" failure for CreateSymbolicLink.
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    match result {
        Ok(()) => true,
        Err(error)
            if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD)
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
