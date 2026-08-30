//! Shell-command confinement on macOS via the Seatbelt kernel sandbox.
//!
//! macOS ships `/usr/bin/sandbox-exec`, which launches a program under a
//! profile written in SBPL — a small s-expression policy language evaluated
//! by the kernel. deep-code composes a deny-by-default profile from the
//! requested [`SandboxPolicy`], passes it with `-p`, and lets the kernel
//! enforce it for the whole process tree the shell spawns. Filesystem paths
//! enter the profile through `-D NAME=value` bindings referenced as
//! `(param "NAME")`, which sidesteps quoting problems with arbitrary paths.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::policy::SandboxPolicy;

/// Launcher that applies an SBPL profile to a child process.
pub const SEATBELT_BINARY: &str = "/usr/bin/sandbox-exec";

/// Whether Seatbelt confinement can actually be used on this host.
///
/// Existence of the binary is not enough: some managed environments and
/// nested sandboxes forbid `sandbox-exec` itself. The probe therefore runs a
/// no-op command under a permissive profile once and caches the verdict for
/// the process lifetime.
pub fn is_available() -> bool {
    static VERDICT: OnceLock<bool> = OnceLock::new();
    *VERDICT.get_or_init(probe_seatbelt)
}

fn probe_seatbelt() -> bool {
    if !Path::new(SEATBELT_BINARY).is_file() {
        return false;
    }
    Command::new(SEATBELT_BINARY)
        .arg("-p")
        .arg("(version 1)(allow default)")
        .arg("--")
        .arg("/usr/bin/true")
        .status()
        .is_ok_and(|status| status.success())
}

/// Build a `Command` that runs `command` through `sh -c` under a Seatbelt
/// profile derived from `policy`, with the granted roots (and `cwd`, when
/// distinct) as the writable roots.
pub fn wrap_shell_command(
    command: &str,
    cwd: &Path,
    granted_roots: &[PathBuf],
    policy: &SandboxPolicy,
) -> Command {
    let profile = compose_profile(policy, granted_roots, cwd);

    let mut launcher = Command::new(SEATBELT_BINARY);
    launcher.arg("-p").arg(profile.render());
    for (name, path) in &profile.bindings {
        launcher.arg(format!("-D{name}={}", path.display()));
    }
    launcher.arg("--").arg("sh").arg("-c").arg(command);
    launcher.current_dir(cwd);
    launcher
}

/// An SBPL profile under construction: rule lines plus the path parameters
/// the rules refer to. Keeping rules and bindings in one place guarantees a
/// rule never mentions a parameter that was not passed on the command line
/// (sandbox-exec refuses to load such a profile).
struct SeatbeltProfile {
    directives: Vec<String>,
    bindings: Vec<(String, PathBuf)>,
}

impl SeatbeltProfile {
    /// Start from the SBPL preamble: profile version and deny-by-default, so
    /// everything below is an explicit grant.
    fn deny_by_default() -> Self {
        Self {
            directives: vec!["(version 1)".to_string(), "(deny default)".to_string()],
            bindings: Vec::new(),
        }
    }

    fn rule(&mut self, sbpl: impl Into<String>) {
        self.directives.push(sbpl.into());
    }

    /// Register a path parameter and return the SBPL fragment referencing it.
    fn bind_path(&mut self, name: &str, path: PathBuf) -> String {
        self.bindings.push((name.to_string(), path));
        format!("(param \"{name}\")")
    }

    fn render(&self) -> String {
        self.directives.join("\n")
    }
}

/// Translate a [`SandboxPolicy`] into a concrete profile.
///
/// Ordering matters in SBPL (a later matching rule wins), so grants and
/// denials are appended in a fixed sequence: process baseline, blanket read,
/// optional network, the writable roots the policy grants, and LAST the
/// credential-directory write denials so no writable root can override them.
fn compose_profile(
    policy: &SandboxPolicy,
    granted_roots: &[PathBuf],
    cwd: &Path,
) -> SeatbeltProfile {
    let mut profile = SeatbeltProfile::deny_by_default();

    // Shell commands are process trees: the shell itself plus whatever it
    // spawns. Fork/exec must work, and members of the same sandbox need to
    // signal and inspect each other (build tools wait on and kill children).
    profile.rule("(allow process-exec)");
    profile.rule("(allow process-fork)");
    profile.rule("(allow signal (target same-sandbox))");
    profile.rule("(allow process-info* (target same-sandbox))");

    // Plenty of stock CLI tools consult defaults(1) domains and sysctl values
    // during startup; blocking these produces confusing mid-command crashes
    // rather than useful denials.
    profile.rule("(allow user-preference-read)");
    profile.rule("(allow sysctl-read)");

    // POSIX semaphores back common runtime primitives (e.g. interpreter
    // worker pools) even for commands that never look parallel.
    profile.rule("(allow ipc-posix-sem)");

    // Terminal plumbing: tools probe for a TTY and may allocate pseudo
    // terminals even when running inside a pipeline.
    profile.rule("(allow pseudo-tty)");
    profile.rule("(allow file-read* file-write* file-ioctl (literal \"/dev/ptmx\"))");

    // Entropy for TLS handshakes, UUIDs, temp-name generation.
    profile.rule("(allow file-read* (literal \"/dev/urandom\"))");

    // Discarding output must always work — but only to the real character
    // device, so a file smuggled in at that path cannot become writable.
    profile.rule(
        "(allow file-write-data (require-all (path \"/dev/null\") \
         (vnode-type CHARACTER-DEVICE)))",
    );

    // Reads are broad: deep-code's own read tools already expose the filesystem
    // to the model, and toolchains (dyld, locale data, SSH keys for `git push`)
    // need it. When network is granted, this broad read is a real exfiltration
    // surface — an approved command could read a secret and POST it out. We do
    // NOT blanket-deny credential reads here because that breaks the very
    // commands the network grant exists for (ssh reading `~/.ssh` for a push).
    // The one unconditional read-deny is deep-code's OWN secret store, added
    // after this line so last-match-wins makes it stick (see below).
    profile.rule("(allow file-read*)");

    // Network is opt-in per policy. `system-socket` covers the raw/system
    // sockets some resolvers use.
    if policy.has_network_access() {
        profile.rule("(allow network-outbound)");
        profile.rule("(allow network-inbound)");
        profile.rule("(allow system-socket)");
    }

    // Grant writes only under the roots the policy hands out (the granted
    // roots and, when different, the command's cwd). Paths are canonicalized
    // because Seatbelt matches the real path — on macOS /tmp is a symlink into
    // /private, and an uncanonicalized grant there would never match.
    let mut granted: Vec<PathBuf> = Vec::new();
    for root in policy.writable_roots(granted_roots, cwd) {
        let resolved = crate::paths::canonicalize(&root).unwrap_or(root);
        if !granted.contains(&resolved) {
            granted.push(resolved);
        }
    }
    for (index, root) in granted.into_iter().enumerate() {
        let param = profile.bind_path(&format!("WRITE_ROOT_{index}"), root);
        profile.rule(format!("(allow file-write* (subpath {param}))"));
    }

    // Credential stores must never be *modified* by a sandboxed command, even
    // inside an otherwise writable tree. Because a later matching rule wins,
    // these denials MUST come after the write grants above — a writable root
    // that is an ancestor of `~/.ssh` (e.g. running with HOME as the
    // workspace) would otherwise override the denial.
    for (name, dir) in credential_dirs() {
        let param = profile.bind_path(&name, dir);
        profile.rule(format!("(deny file-write* (subpath {param}))"));
    }

    // A single-component entry (`~/.ssh`) is fully sealed by the subpath deny
    // above: it covers the entry's own inode, so renaming or unlinking it is
    // refused along with writing inside it. A multi-component entry
    // (`~/.config/gh`, `~/Library/Keychains`) is not — its parent is an
    // ordinary writable directory when HOME is a granted root, so the deny on
    // the leaf is walked around by moving that parent aside and back:
    //
    //     mv ~/Library ~/L && echo 'secret' > ~/L/Keychains/… \
    //         && mv ~/L ~/Library
    //
    // Deny unlink on each intermediate directory to close it — a `literal`, so
    // only that directory's own rename is refused while the ordinary writes
    // beneath it a working tool needs stay allowed, the narrowest rule that
    // works. Enumerated from the entry list (see `credential_parent_dirs_under`)
    // rather than hand-picked, so a new multi-component entry is covered on the
    // spot instead of the next time someone remembers this block exists.
    if let Some(home) = crate::paths::home_dir() {
        for (index, dir) in credential_parent_dirs_under(&home).into_iter().enumerate() {
            let param = profile.bind_path(&format!("KEEP_PARENT_{index}"), dir);
            profile.rule(format!("(deny file-write-unlink (literal {param}))"));
        }
    }

    // deep-code's OWN config dir holds the plaintext API key. No sandboxed
    // subprocess ever needs to touch it, so deny BOTH write and read — this is
    // the one credential store we can fully seal without breaking legitimate
    // tools, and it closes the highest-value target: a single approved network
    // command reading the key off disk, or rewriting `provider.base_url` to a
    // proxy. Read-deny is rendered after `(allow file-read*)` so it wins.
    // Canonicalized for the same reason as `credential_dirs_under`: an
    // unresolved spelling is a deny the kernel never matches.
    if let Some(home) = crate::paths::home_dir() {
        let home = crate::paths::canonicalize(&home).unwrap_or(home);
        let joined = home.join(crate::paths::DEEP_CODE_DIR);
        let resolved = crate::paths::canonicalize(&joined)
            .ok()
            .filter(|path| *path != joined);
        for (name, dir) in std::iter::once(("KEEP_DEEP_CODE", joined))
            .chain(resolved.map(|path| ("KEEP_DEEP_CODE_R", path)))
        {
            let param = profile.bind_path(name, dir);
            profile.rule(format!("(deny file-write* (subpath {param}))"));
            profile.rule(format!("(deny file-read* (subpath {param}))"));
        }
    }

    profile
}

/// Home directories/files holding long-lived secrets that stay write-denied
/// under every policy: SSH keys, cloud credentials, GnuPG keyrings, `.netrc`
/// passwords, and the token stores of common dev tools (gh, docker, kube, npm,
/// pip, git). Reads of these are intentionally left open — some are needed by
/// the very network commands the sandbox now permits (`ssh` reads `~/.ssh` for
/// `git push`); the residual read-exfiltration risk is accepted and documented.
/// deep-code's own `~/.deep-code` is handled separately (read+write denied).
/// Empty when `HOME` is unset (nothing to protect, nothing to bind).
///
/// The entry list lives in [`crate::paths::CREDENTIAL_ENTRIES`], shared with
/// the write-grant floor in `workspace_policy::resolve_grant_target`: the
/// kernel fence here and the in-process path fence there must refuse the same
/// set, or a grant could hand over what this profile denies.
fn credential_dirs() -> Vec<(String, PathBuf)> {
    let Some(home) = crate::paths::home_dir() else {
        return Vec::new();
    };
    credential_dirs_under(&home)
}

/// The credential denials for a given home, both spellings where they differ.
///
/// Canonicalized, because Seatbelt matches the path the kernel resolves. A deny
/// bound to an unresolved `$HOME` silently fails to match wherever HOME or the
/// entry itself traverses a symlink — and dotfile managers routinely symlink
/// `~/.aws`, `~/.kube`, `~/.netrc` or `~/.git-credentials` into a checkout. The
/// write GRANTS in this same file were already canonicalized for exactly this
/// reason (`/tmp` is a symlink into `/private` on macOS); the denials were not,
/// so the kernel fence that the in-process grant floor defers to could be a
/// no-op while the floor itself still refused — two fences, two answers, and
/// the weaker one was the one holding the key material.
///
/// Both the joined and the resolved spelling are emitted when they differ: a
/// sandboxed command can reach the store by either name. Split from
/// [`credential_dirs`] so the rule is testable without touching the real HOME.
fn credential_dirs_under(home: &Path) -> Vec<(String, PathBuf)> {
    // A home that cannot be resolved still gets denials, at its unresolved
    // spelling — losing the fence entirely is the worse failure.
    let resolved_home = crate::paths::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut dirs = Vec::new();
    for (index, entry) in crate::paths::CREDENTIAL_ENTRIES.iter().enumerate() {
        let joined = resolved_home.join(entry);
        // Resolves through an intermediate symlink even when the leaf does not
        // exist yet: for a multi-component entry (`.config/gh`,
        // `Library/Keychains`) behind a dotfiles-managed parent the
        // all-or-nothing `canonicalize` gives up, leaving the real location
        // undenied unless the existing prefix is resolved on its own.
        if let Some(resolved) = crate::paths::canonicalize_existing_prefix(&joined)
            && resolved != joined
        {
            dirs.push((format!("KEEP_{index}_R"), resolved));
        }
        dirs.push((format!("KEEP_{index}"), joined));
    }
    dirs
}

/// The intermediate directories of every multi-component credential entry, so
/// each can be denied `file-write-unlink`.
///
/// `(deny file-write* (subpath X))` covers X and everything beneath it,
/// including X's own inode, so a single-component entry (`~/.ssh`) is fully
/// sealed: renaming or unlinking it is refused along with writing inside it. A
/// multi-component entry is not — its parent is an ordinary writable directory
/// when HOME is a granted root, so the deny on the leaf is walked around by
/// moving that parent aside and back:
///
/// ```text
/// mv ~/Library ~/L && echo … > ~/L/Keychains/… && mv ~/L ~/Library
/// ```
///
/// Locking each intermediate directory against unlink closes that walk-around.
/// Deduplicated, since `.config/gh` and `.config/gcloud` share `~/.config`. The
/// paths are the literal spelling under the resolved home — a rename acts on
/// the name, not on wherever a symlinked name points, and a symlinked leaf is
/// already covered by the resolved spelling `credential_dirs_under` emits.
///
/// Derived from [`crate::paths::CREDENTIAL_ENTRIES`], not hand-listed: adding
/// `Library/Keychains` to that list is what surfaced this gap, so a future
/// multi-component entry must not depend on someone remembering to lock its
/// parent by hand.
fn credential_parent_dirs_under(home: &Path) -> Vec<PathBuf> {
    let resolved_home = crate::paths::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in crate::paths::CREDENTIAL_ENTRIES {
        // Every ancestor between the resolved home and the leaf; the leaf
        // itself is left out, sealed by its own subpath deny.
        let components: Vec<_> = Path::new(entry).components().collect();
        let mut prefix = resolved_home.clone();
        for component in &components[..components.len().saturating_sub(1)] {
            prefix.push(component.as_os_str());
            if !dirs.contains(&prefix) {
                dirs.push(prefix.clone());
            }
        }
    }
    dirs
}

#[cfg(test)]
mod tests;
