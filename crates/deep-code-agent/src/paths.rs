//! Shared filesystem locations.

use std::path::{Path, PathBuf};

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
///
/// Public because the boundary is decided in two crates. Anything that
/// produces a path the write boundary will later be compared against — the
/// TUI's `--add-dir`, most of all, whose value is *signed into the session
/// record* — must use this reading and not `Path::canonicalize`, or the two
/// spellings of the same directory stop being equal and a perfectly good
/// grant is dropped on the next `-c` as "no longer the directory that was
/// approved".
pub fn canonicalize(path: &std::path::Path) -> std::io::Result<PathBuf> {
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

/// Create the chain of directories deep-code owns at `deepest`, refusing to
/// accept a symlink at any level.
///
/// `levels` counts upward from `deepest` inclusive, and stops there on purpose:
/// above our own directories the path belongs to the user, and a project living
/// behind a symlinked parent is a normal setup, not an attack.
///
/// The rule this enforces is that **every directory we own must be a real
/// directory**. `create_dir_all` alone follows a symlink at any component, so a
/// repository that ships `.deep-code` as a link to somewhere else silently
/// relocated everything written under it — session transcripts, the stderr log,
/// checkpoints — outside the workspace, from the unsandboxed parent process.
/// Planting that link is an ordinary permitted write inside a granted root, so
/// no sandbox on any platform refuses it; this is the check that does.
///
/// `symlink_metadata`, never `metadata`: the latter resolves the link and then
/// answers about its target, which is the question that lets the link through.
pub fn ensure_owned_dirs(deepest: &std::path::Path, levels: usize) -> std::io::Result<()> {
    let owned: Vec<_> = deepest.ancestors().take(levels).collect();
    for dir in owned.into_iter().rev() {
        ensure_real_dir(dir)?;
    }
    Ok(())
}

/// The directory must exist and be a real directory, or be created as one.
fn ensure_real_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} is a symlink or a file, not a directory", dir.display()),
        )),
        Err(_) => create_private_dir(dir),
    }
}

fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    // `mut` is consumed by the unix-only `mode` call below; Windows builds
    // see it unused and clippy runs with `-D warnings` there.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => Ok(()),
        // Lost the race to a concurrent writer — fine, as long as what landed
        // there is a real directory and not a link planted meanwhile.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(dir) {
                Ok(meta) if meta.is_dir() => Ok(()),
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Home-relative locations holding long-lived secrets: SSH keys, cloud
/// credentials, GnuPG keyrings, `.netrc` passwords, and the token stores of
/// common dev tools.
///
/// [`sensitive_paths`] turns this list into a refusal in the model-facing grant
/// channel, on every platform. The **macOS** sandbox additionally turns each
/// entry into a `deny file-write*` that outranks every writable root — but that
/// second fence is macOS-only: Landlock is allow-list-only and cannot express a
/// denial inside a granted root, and Windows has no filesystem confinement at
/// all. So this is one list with one guaranteed enforcer and a second on macOS,
/// not one list behind two fences everywhere; a granted root that CONTAINS a
/// credential store still reaches it on Linux and Windows. (`--add-dir` is
/// deliberately exempt from the floor anyway — see
/// `workspace_policy::refuse_as_unattended_root`.)
///
/// Scope: credential *stores*, not everything that converts to code execution.
/// `~/.cargo`, `~/Library/LaunchAgents` and `~/.gitconfig` are absent on
/// purpose — see `session_integrity`, which argues that no enumeration of
/// dangerous directories can be complete and authenticates the author of a
/// grant instead of judging its path. What this list must not do is be
/// *inconsistent* within its own category, which is why the cloud trio is
/// AWS/GCP/Azure rather than AWS alone.
pub(crate) const CREDENTIAL_ENTRIES: &[&str] = &[
    ".ssh",
    ".aws",
    ".config/gcloud",
    ".azure",
    ".gnupg",
    ".netrc",
    ".config/gh",
    ".docker",
    ".kube",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    // Sibling agents' token stores, for the same reason `~/.deep-code` is
    // covered: an OAuth token plus a hooks/settings file that runs commands.
    ".claude",
    // The largest single secret store on the project's primary platform.
    "Library/Keychains",
];

/// Ecosystem cache directories a sandboxed command legitimately needs to
/// WRITE, and that nothing else grants.
///
/// The same argument that puts the temp dir in `SandboxPolicy::writable_roots`
/// applies here and only here: these are directories the user's own toolchain
/// writes unconditionally, so a profile without them does not *confine*
/// `npm install` or a cold `cargo build` — it makes them fail (the EPERM-on-its-
/// own-cache case `sandbox::write_denial_signature` was taught to recognize).
///
/// **Subpaths, never the tool's home.** `$CARGO_HOME/config.toml` selects a
/// source replacement (code execution at the next build) and `~/.npmrc` carries
/// a registry token — both stay out, and `~/.npmrc` is in
/// [`CREDENTIAL_ENTRIES`] besides. Granting the cache is not granting the tool.
///
/// **Paths must exist to be granted**, and a missing one is therefore *created*
/// — not silently dropped, and never by granting a parent instead.
///
/// Landlock has no way to express a rule for an absent path (adding one errors
/// and the whole per-command ruleset fails with it), and an unresolvable
/// Seatbelt rule is a rule the kernel never matches. Dropping the entry is what
/// this used to do, and the case it dropped is the common one: a fresh machine,
/// a fresh CI container, a first `npm install`. `~/.npm/_cacache` does not exist
/// yet, the tool cannot create it because `~/.npm` is not writable either, and
/// the run dies on the very EPERM these roots were added to prevent — in the
/// default configuration, on a host where nothing was misconfigured.
///
/// So the leaf is created here when its **parent already exists**. Creating it
/// is what the toolchain would do on first use, and it is strictly narrower than
/// the alternative (granting `~/.npm`, or `$CARGO_HOME` — which would expose
/// `config.toml`, the source-replacement channel, which is exactly why a parent
/// grant is never an option here). A parent that does not exist means this
/// machine never used that toolchain, and then nothing is created and nothing is
/// granted: an agent must not bring `~/.npm` into being for a user who has never
/// run npm.
pub(crate) fn tool_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        roots.push((cargo_home.join("registry"), CacheLeaf::Directory));
        // A file, not a directory: cargo's advisory lock. `create_dir_all` here
        // would leave a *directory* where cargo expects a file, so the kind
        // travels with the path.
        roots.push((cargo_home.join(".package-cache"), CacheLeaf::File));
    }
    if let Some(home) = home_dir() {
        roots.push((home.join(".npm").join("_cacache"), CacheLeaf::Directory));
        // The browser download root, where the platform actually puts it.
        #[cfg(target_os = "macos")]
        roots.push((
            home.join("Library").join("Caches").join("ms-playwright"),
            CacheLeaf::Directory,
        ));
        #[cfg(not(target_os = "macos"))]
        roots.push((
            home.join(".cache").join("ms-playwright"),
            CacheLeaf::Directory,
        ));
    }

    roots
        .into_iter()
        .filter_map(|(path, leaf)| leaf.ensure(path))
        .collect()
}

/// Whether one cache path is a directory or a file, which decides how it is
/// created. Only cargo's lock is a file today; the distinction is carried rather
/// than guessed because guessing wrong turns a lock file into a directory.
#[derive(Clone, Copy)]
enum CacheLeaf {
    Directory,
    File,
}

impl CacheLeaf {
    /// The path, once it exists — creating it when the parent is there to hold
    /// it, and giving up when it is not.
    fn ensure(self, path: PathBuf) -> Option<PathBuf> {
        if path.exists() {
            return Some(path);
        }
        if !path.parent().is_some_and(Path::exists) {
            return None;
        }
        let created = match self {
            Self::Directory => std::fs::create_dir_all(&path).is_ok(),
            Self::File => std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .is_ok(),
        };
        created.then_some(path)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Subpaths of the tools' homes, never the homes themselves:
    /// `$CARGO_HOME/config.toml` selects a source replacement (code execution at
    /// the next build) and `~/.npmrc` carries a registry token — and `~/.npmrc`
    /// is in [`CREDENTIAL_ENTRIES`] besides. Granting the cache must not grant
    /// the tool.
    #[test]
    fn tool_cache_roots_never_grant_a_tool_config_or_home() {
        for root in tool_cache_roots() {
            let name = root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            assert_ne!(
                name, "config.toml",
                "{root:?} would select a source replacement"
            );
            assert_ne!(name, ".npmrc", "{root:?} would carry a registry token");
            // `Path::ends_with` compares whole components, so this is the home
            // itself and not merely a path containing `.cargo`.
            for tool_home in [".cargo", ".npm"] {
                assert!(
                    !root.ends_with(tool_home),
                    "{root:?} is the tool home itself"
                );
            }
        }
    }

    /// Only paths that exist. Landlock cannot express a rule for an absent path
    /// (adding one fails the whole per-command ruleset, taking every other
    /// margin with it), and a Seatbelt rule whose path does not resolve is a rule
    /// the kernel never matches — so a missing entry is dropped, not bound.
    #[test]
    fn tool_cache_roots_only_returns_paths_that_exist() {
        assert!(tool_cache_roots().iter().all(|root| root.exists()));
    }

    /// The fresh-machine case, which is the one that used to fail: an absent
    /// cache leaf is created when — and only when — its parent is already there.
    /// Dropping it instead meant the FIRST `npm install` on a new machine died
    /// on the EPERM these roots exist to prevent, in the default configuration,
    /// with nothing misconfigured.
    #[test]
    fn a_missing_cache_leaf_is_created_only_where_its_parent_exists() {
        let parent = tempfile::tempdir().unwrap();

        let leaf = parent.path().join("_cacache");
        assert_eq!(
            CacheLeaf::Directory.ensure(leaf.clone()),
            Some(leaf.clone())
        );
        assert!(leaf.is_dir(), "the directory kind makes a directory");

        // …and the file kind must NOT: `create_dir_all` on cargo's advisory lock
        // would leave a directory where cargo expects a file.
        let lock = parent.path().join(".package-cache");
        assert_eq!(CacheLeaf::File.ensure(lock.clone()), Some(lock.clone()));
        assert!(lock.is_file(), "the file kind makes a file");

        // Already present: handed back untouched.
        assert_eq!(CacheLeaf::Directory.ensure(leaf.clone()), Some(leaf));

        // Parent absent: nothing created, nothing granted. An agent must not
        // bring `~/.npm` into being for a user who has never run npm.
        let deep = parent.path().join("missing").join("_cacache");
        assert_eq!(CacheLeaf::Directory.ensure(deep.clone()), None);
        assert!(!deep.exists());
    }
}
