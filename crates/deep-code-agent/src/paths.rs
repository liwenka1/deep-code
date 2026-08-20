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

/// deep-code's own per-user directory, holding the global config — which is
/// the trust root: `api_key` in plaintext, and `approval.auto_allow`, honoured
/// *only* from this layer (a project config is refused, see
/// `config::layers`). Writing this file is therefore not a session-scoped act.
pub(crate) const DEEP_CODE_DIR: &str = ".deep-code";

/// Home-relative locations holding long-lived secrets: SSH keys, cloud
/// credentials, GnuPG keyrings, `.netrc` passwords, and the token stores of
/// common dev tools. The macOS sandbox turns each of these into a
/// `deny file-write*` that outranks every writable root, and
/// [`sensitive_paths`] turns the same list into a refusal in the model-facing
/// grant channel — one list, so the kernel fence and the tool fence cannot
/// disagree about what counts as a credential store.
pub(crate) const CREDENTIAL_ENTRIES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".config/gh",
    ".docker",
    ".kube",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
];

/// Absolute paths that a model-requested write grant must never reach: the
/// [`CREDENTIAL_ENTRIES`] plus deep-code's own [`DEEP_CODE_DIR`].
///
/// Both spellings of each entry are returned when they differ — the joined
/// one and its resolved form — because the caller compares against a
/// canonical candidate, and a credential store that is itself a symlink has
/// to be refused by its real location too. Entries that do not exist on this
/// host are still returned: they must not become grantable simply because the
/// user has not created them yet, or the first grant would be the thing that
/// makes `~/.ssh` writable.
///
/// Empty when the environment names no home (nothing to locate, nothing to
/// protect).
pub(crate) fn sensitive_paths() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    // A home that will not canonicalize still gets a floor, at its unresolved
    // spelling. Dropping the whole list here removed the credential floor
    // entirely — the wrong direction to fail for the one check standing between
    // a requested grant and the plaintext API key.
    let home = home.canonicalize().unwrap_or(home);
    let mut paths = Vec::new();
    for entry in CREDENTIAL_ENTRIES
        .iter()
        .copied()
        .chain(std::iter::once(DEEP_CODE_DIR))
    {
        let joined = home.join(entry);
        if let Ok(resolved) = joined.canonicalize()
            && resolved != joined
        {
            paths.push(resolved);
        }
        paths.push(joined);
    }
    paths
}
