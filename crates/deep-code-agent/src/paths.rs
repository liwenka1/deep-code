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

/// The data-volume alias macOS splices into the system volume's namespace.
#[cfg(target_os = "macos")]
const FIRMLINK_DATA_PREFIX: &str = "/System/Volumes/Data";

/// `Path::canonicalize`, then brought back into the ONE namespace every floor
/// in this crate is written in.
///
/// On macOS `/Users/x` and `/System/Volumes/Data/Users/x` are the same
/// directory — same device, same inode — because the data volume is firmlinked
/// into the read-only system volume. `realpath(3)` does **not** collapse one
/// spelling into the other: each canonicalizes to itself. Every floor here
/// compares canonical paths with `starts_with`, and that prefix test then
/// misses in both directions — a grant requested at the Data spelling is not
/// "inside the home directory", does not "overlap a credential store", and is
/// not `~/.deep-code`, while writing through it lands on exactly those files.
/// The kernel fence does not cover the gap either: Seatbelt normalizes
/// firmlinks, but `read_file`/`write_file` are in-process and never meet it.
///
/// The prefix is stripped only when the shorter spelling names the very same
/// inode, so a directory that merely happens to live under
/// `/System/Volumes/Data` keeps its own identity. Everywhere else this is
/// plain `canonicalize`.
pub(crate) fn canonicalize(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    #[cfg(target_os = "macos")]
    {
        Ok(strip_firmlink(resolved))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(resolved)
    }
}

#[cfg(target_os = "macos")]
fn strip_firmlink(path: PathBuf) -> PathBuf {
    use std::os::unix::fs::MetadataExt;

    let Ok(rest) = path.strip_prefix(FIRMLINK_DATA_PREFIX) else {
        return path;
    };
    let stripped = std::path::Path::new("/").join(rest);
    let same_inode = match (stripped.metadata(), path.metadata()) {
        (Ok(short), Ok(long)) => short.dev() == long.dev() && short.ino() == long.ino(),
        _ => false,
    };
    if same_inode { stripped } else { path }
}

/// deep-code's own per-user directory, holding the global config — which is
/// the trust root: `api_key` in plaintext, and `approval.auto_allow`, honoured
/// *only* from this layer (a project config is refused, see
/// `config::layers`). Writing this file is therefore not a session-scoped act.
pub(crate) const DEEP_CODE_DIR: &str = ".deep-code";

/// [`canonicalize`], but for a path that need not exist yet: resolve the
/// deepest ancestor that *does* exist and re-append the rest.
///
/// `Path::canonicalize` is all-or-nothing, and only one credential entry has
/// an intermediate component — `.config/gh`. So on the common dotfile-manager
/// layout where `~/.config` is a symlink into `~/dotfiles/config` and `gh`
/// has not been created yet, resolving the whole path fails and only the
/// unresolved `$HOME/.config/gh` reaches the floor. `~/dotfiles/config` is
/// then grantable in both directions of the overlap test, and the Seatbelt
/// deny misses it too — defeating the stated intent that an entry must not
/// become reachable merely because the user has not created it yet.
pub(crate) fn canonicalize_existing_prefix(path: &std::path::Path) -> Option<PathBuf> {
    let mut trailing = Vec::new();
    let mut probe = path;
    loop {
        if let Ok(resolved) = canonicalize(probe) {
            let mut out = resolved;
            out.extend(trailing.iter().rev());
            return Some(out);
        }
        let name = probe.file_name()?;
        trailing.push(name.to_os_string());
        probe = probe.parent()?;
    }
}

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
    let home = canonicalize(&home).unwrap_or(home);
    let mut paths = Vec::new();
    for entry in CREDENTIAL_ENTRIES
        .iter()
        .copied()
        .chain(std::iter::once(DEEP_CODE_DIR))
    {
        let joined = home.join(entry);
        // Resolves through an intermediate symlink even when the leaf does
        // not exist yet — `.config/gh` behind a dotfiles-managed `~/.config`
        // is the case that matters.
        if let Some(resolved) = canonicalize_existing_prefix(&joined)
            && resolved != joined
        {
            paths.push(resolved);
        }
        paths.push(joined);
    }
    paths
}
