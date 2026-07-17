//! Shared filesystem locations.

use std::path::PathBuf;

/// The user's home directory, if the environment names one.
///
/// `HOME` first (Unix, and respected when set on Windows), then `USERPROFILE`
/// (the usual Windows spelling). Every global per-user path in the crate —
/// config, hooks log, skills — must resolve through this one
/// helper: if two call sites disagreed about what "home" is, a value written
/// by one feature would silently be invisible to another.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
