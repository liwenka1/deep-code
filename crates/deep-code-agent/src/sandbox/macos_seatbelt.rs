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
mod tests {
    use super::*;

    /// Single-root granted list, the shape every pre-`--add-dir` call had.
    fn single_root(ws: &Path) -> Vec<PathBuf> {
        vec![ws.to_path_buf()]
    }

    /// A symlinked credential store is denied by the path the kernel resolves,
    /// not only by the spelling under `$HOME`.
    ///
    /// Seatbelt matches resolved paths, so `(deny file-write* (subpath
    /// $HOME/.aws))` never fires when `~/.aws` is a symlink into a dotfiles
    /// checkout — which is how dotfile managers set these up. The write grants
    /// in this file were already canonicalized for exactly this reason; the
    /// denials were not, leaving the kernel fence that the in-process grant
    /// floor defers to as a silent no-op.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_credential_store_is_denied_by_its_resolved_path() {
        let home = tempfile::TempDir::new().unwrap();
        let home = home.path().canonicalize().unwrap();
        let real = tempfile::TempDir::new().unwrap();
        let real = real.path().canonicalize().unwrap();
        // `~/.aws` → a directory outside home, the dotfiles-manager shape.
        std::os::unix::fs::symlink(&real, home.join(".aws")).unwrap();

        let dirs = credential_dirs_under(&home);
        let paths: Vec<&Path> = dirs.iter().map(|(_, dir)| dir.as_path()).collect();
        assert!(
            paths.contains(&real.as_path()),
            "the resolved store must be denied too: {paths:?}"
        );
        assert!(
            paths.contains(&home.join(".aws").as_path()),
            "and the spelling under home stays denied: {paths:?}"
        );
        // Parameter names must stay distinct, or one binding overwrites the
        // other and only a single deny is emitted.
        let names: std::collections::BTreeSet<&str> =
            dirs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names.len(), dirs.len(), "duplicate SBPL parameter name");
    }

    #[test]
    fn availability_probe_is_stable_and_panic_free() {
        // Two calls must agree (the verdict is cached process-wide).
        assert_eq!(is_available(), is_available());
    }

    #[test]
    fn profile_opens_with_deny_by_default() {
        let ws = Path::new("/tmp/dc-ws");
        let text =
            compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws).render();
        assert!(text.starts_with("(version 1)\n(deny default)"));
        assert!(text.contains("(allow file-read*)"));
    }

    /// Every credential entry with an intermediate component gets that
    /// component locked, so the subpath deny on the leaf cannot be walked
    /// around by renaming the parent aside. `~/.config` (shared by gh and
    /// gcloud) and `~/Library` (Keychains) are the cases today; a
    /// single-component entry contributes nothing, being sealed by its own
    /// subpath deny.
    ///
    /// Enumerated from `CREDENTIAL_ENTRIES` so a future multi-component entry
    /// is pinned automatically — a hand-listed subset is exactly how
    /// `~/Library` came to be missing after `Library/Keychains` was added.
    #[test]
    fn every_multi_component_credential_parent_is_locked() {
        let home = tempfile::TempDir::new().unwrap();
        let home = home.path().canonicalize().unwrap();

        let parents = credential_parent_dirs_under(&home);

        for entry in crate::paths::CREDENTIAL_ENTRIES {
            // Walk every ancestor between the leaf and home; each must be
            // locked. Handles deeper nesting too, not just two-level entries.
            let mut ancestor = Path::new(entry).parent();
            while let Some(rel) = ancestor {
                if rel.as_os_str().is_empty() {
                    break;
                }
                let abs = home.join(rel);
                assert!(
                    parents.contains(&abs),
                    "ancestor {} of entry {entry:?} must be locked against rename: {parents:?}",
                    abs.display()
                );
                ancestor = rel.parent();
            }
        }
        // Leaves are NOT here: they are sealed by their own subpath deny, and
        // locking them would forbid the legitimate writes a tool makes inside.
        assert!(
            !parents.contains(&home.join(".config/gh")),
            "a leaf must not be locked here: {parents:?}"
        );
        // No duplicates: gh and gcloud share `~/.config`, locked once.
        let unique: std::collections::BTreeSet<&PathBuf> = parents.iter().collect();
        assert_eq!(
            unique.len(),
            parents.len(),
            "duplicate parent lock: {parents:?}"
        );
    }

    /// The profile must actually emit a `file-write-unlink` deny for each
    /// intermediate directory (and only unlink, so the tree does not turn
    /// read-only). The regression this guards: `~/Library`, the parent of the
    /// newly added `Library/Keychains`, must be one of them — not just
    /// `~/.config`, which was the only one the previous hand-coded block knew.
    #[test]
    fn intermediate_credential_dirs_are_locked_against_rename() {
        let Some(home) = crate::paths::home_dir() else {
            eprintln!("no home dir on this host; skipping");
            return;
        };
        let ws = Path::new("/tmp/dc-ws");
        let profile = compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws);
        let text = profile.render();

        for dir in credential_parent_dirs_under(&home) {
            let param = profile
                .bindings
                .iter()
                .find(|(_, path)| *path == dir)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| {
                    panic!("{} must be bound: {:?}", dir.display(), profile.bindings)
                });
            assert!(
                text.contains(&format!(
                    "(deny file-write-unlink (literal (param \"{param}\")))"
                )),
                "renaming {} must be refused, or its nested store's deny is walked around: {text}",
                dir.display()
            );
            // Only unlink — ordinary writes elsewhere under it stay allowed.
            assert!(
                !text.contains(&format!("(deny file-write* (subpath (param \"{param}\")))")),
                "the whole of {} must not become read-only: {text}",
                dir.display()
            );
        }

        // The specific regression: Keychains' parent is locked, not only
        // `~/.config`.
        let library = crate::paths::canonicalize(&home)
            .unwrap_or(home)
            .join("Library");
        assert!(
            profile.bindings.iter().any(|(_, path)| *path == library),
            "~/Library must be locked so Keychains' deny cannot be moved out from under it: {:?}",
            profile.bindings
        );
    }

    #[test]
    fn network_rules_appear_only_when_policy_grants_network() {
        let ws = Path::new("/tmp/dc-ws");
        let offline =
            compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws).render();
        assert!(!offline.contains("network-outbound"));

        let online = compose_profile(
            &SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
            &single_root(ws),
            ws,
        )
        .render();
        assert!(online.contains("(allow network-outbound)"));
        assert!(online.contains("(allow network-inbound)"));
    }

    #[test]
    fn distinct_cwd_receives_its_own_write_grant() {
        let workspace = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            &single_root(workspace.path()),
            elsewhere.path(),
        );
        let write_roots: Vec<&str> = profile
            .bindings
            .iter()
            .filter(|(name, _)| name.starts_with("WRITE_ROOT_"))
            .map(|(name, _)| name.as_str())
            .collect();
        // Workspace, the distinct cwd, the temp dir, and /tmp — see
        // `SandboxPolicy::writable_roots` for why the last two are mandatory.
        assert_eq!(
            write_roots,
            [
                "WRITE_ROOT_0",
                "WRITE_ROOT_1",
                "WRITE_ROOT_2",
                "WRITE_ROOT_3"
            ]
        );
        assert!(profile.render().contains("WRITE_ROOT_1"));
    }

    #[test]
    fn extra_granted_root_receives_its_own_write_grant() {
        // An `--add-dir` grant must surface as a real write root in the
        // profile — this is the kernel half of the multi-root feature; the
        // tool layer's WorkspacePolicy is the other half.
        let workspace = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let granted = vec![workspace.path().to_path_buf(), extra.path().to_path_buf()];
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            &granted,
            workspace.path(),
        );
        let extra_canonical = extra.path().canonicalize().unwrap();
        assert!(
            profile
                .bindings
                .iter()
                .any(|(name, path)| name.starts_with("WRITE_ROOT_") && *path == extra_canonical),
            "extra root must be granted; bindings were {:?}",
            profile.bindings
        );
    }

    #[test]
    fn temp_dir_is_always_a_write_root() {
        // Regression guard: `rustc`, `mktemp` and git's xcrun shim all write to
        // $TMPDIR unconditionally, so dropping this grant does not confine
        // `cargo build`, it breaks it. Both backends read `writable_roots`.
        let workspace = tempfile::tempdir().unwrap();
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            &single_root(workspace.path()),
            workspace.path(),
        );
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert!(
            profile
                .bindings
                .iter()
                .any(|(name, path)| { name.starts_with("WRITE_ROOT_") && *path == temp }),
            "temp dir must be granted; bindings were {:?}",
            profile.bindings
        );
    }

    #[test]
    fn credential_denials_outrank_write_grants() {
        // SBPL resolves conflicts by last-match-wins, so the credential-dir
        // denials must render AFTER every write grant. If a writable root is
        // an ancestor of `~/.ssh` (HOME as workspace), an earlier deny would
        // be silently overridden and sandboxed commands could edit
        // authorized_keys.
        let home = std::env::var("HOME").expect("test environment has HOME");
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            &single_root(home.as_ref()),
            home.as_ref(),
        );
        let text = profile.render();
        let last_grant = text
            .rfind("(allow file-write* (subpath")
            .expect("profile grants a write root");
        let first_denial = text
            .find("(deny file-write* (subpath")
            .expect("profile denies credential dirs");
        assert!(
            first_denial > last_grant,
            "credential denials must come after write grants:\n{text}"
        );
    }

    #[test]
    fn own_config_dir_is_read_and_write_denied() {
        // deep-code's own `~/.deep-code` (plaintext API key) must be denied for
        // BOTH read and write, and the read-deny must render after the blanket
        // `(allow file-read*)` so last-match-wins seals it.
        let home = std::env::var("HOME").expect("test environment has HOME");
        let profile = compose_profile(
            &SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
            &single_root(Path::new("/tmp/dc-ws")),
            Path::new("/tmp/dc-ws"),
        );
        let _ = home;
        let text = profile.render();
        assert!(profile.bindings.iter().any(|(n, _)| n == "KEEP_DEEP_CODE"));
        let allow_read = text.find("(allow file-read*)").expect("blanket read grant");
        let deny_read = text
            .find("(deny file-read* (subpath")
            .expect("own-store read deny");
        assert!(
            deny_read > allow_read,
            "own-store read deny must come after the blanket read allow:\n{text}"
        );
        // And its write is denied too.
        assert!(text.matches("(deny file-write* (subpath").count() >= 2);
    }

    #[test]
    fn credential_param_names_are_legal_and_collision_free() {
        // sandbox-exec takes the LAST `-D` for a repeated name and silently
        // drops the earlier binding, so two entries that fold to the same
        // identifier would evaporate one deny with no error at all. Naming by
        // index makes a collision impossible instead of merely unlikely: the
        // old scheme folded every non-alphanumeric to `_`, so a future
        // `.ssh-r` entry would have collided with `.ssh`'s resolved variant.
        let Some(home) = crate::paths::home_dir() else {
            eprintln!("no home dir on this host; skipping");
            return;
        };
        let dirs = credential_dirs_under(&home);
        let mut names: Vec<&str> = dirs.iter().map(|(name, _)| name.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate -D name would drop a deny");
        for (name, _) in &dirs {
            assert!(
                name.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
                "{name} is not a legal SBPL identifier"
            );
        }
    }

    #[test]
    fn every_referenced_param_is_bound() {
        // sandbox-exec rejects profiles referencing unbound params; make sure
        // rules and bindings cannot drift apart.
        let workspace = tempfile::tempdir().unwrap();
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            &single_root(workspace.path()),
            workspace.path(),
        );
        let text = profile.render();
        for fragment in text.split("(param \"").skip(1) {
            let name = fragment.split('"').next().unwrap();
            assert!(
                profile.bindings.iter().any(|(bound, _)| bound == name),
                "profile references unbound param {name}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_cannot_write_outside_granted_roots() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        // The escape target must sit outside every writable root, so it cannot
        // be a tempdir — the temp dir is itself granted. $HOME is not granted
        // (only its credential subdirs are explicitly denied), so a plain file
        // there is the honest "absence of grant" probe. Nothing is created when
        // the sandbox holds; the cleanup below only matters if it does not.
        let Some(home) = crate::paths::home_dir() else {
            eprintln!("no home dir on this host; skipping");
            return;
        };
        let escape = home.join(format!(
            ".deep-code-sandbox-escape-probe-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&escape);

        let status = wrap_shell_command(
            &format!("printf leaked > {}", escape.display()),
            workspace.path(),
            &single_root(workspace.path()),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandbox-exec should launch");

        let leaked = escape.exists();
        let _ = std::fs::remove_file(&escape);
        assert!(
            !status.success() && !leaked,
            "write outside every writable root must be denied"
        );
    }

    /// Kernel-level pin for the mid-session grant: a directory added to the
    /// live `WorkspacePolicy` via `grant_extra` (the approved
    /// `request_write_root` path) must be writable for the NEXT sandboxed
    /// command — through the same policy clone the shell tool has held since
    /// launch, with no relaunch and no registry rebuild.
    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_writes_into_a_root_granted_mid_session() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        // The grant target must start OUTSIDE every writable root, so it
        // cannot be a tempdir (the temp dir is itself always granted). A
        // HOME subdirectory is the honest "no grant" starting point, exactly
        // like the escape probe in the denial test above.
        let Some(home) = crate::paths::home_dir() else {
            eprintln!("no home dir on this host; skipping");
            return;
        };
        let target = home.join(format!(
            ".deep-code-sandbox-grant-probe-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&target).unwrap();
        let target = target.canonicalize().unwrap();
        let probe = target.join("artifact.txt");

        let policy =
            crate::workspace_policy::WorkspacePolicy::new(workspace.path().to_path_buf()).unwrap();
        let tool_held_clone = policy.clone(); // held since "launch"
        let write_cmd = format!("echo built > {}", probe.display());
        let run = |roots: &[PathBuf]| {
            wrap_shell_command(
                &write_cmd,
                workspace.path(),
                roots,
                &SandboxPolicy::workspace_write(),
            )
            .output()
            .expect("sandbox-exec should launch")
        };

        // Before the grant the kernel refuses the write; after `grant_extra`
        // on the SHARED policy the same spelling succeeds through the clone
        // the tool has held since launch — the widened list reaches the
        // sandbox per-spawn, no relaunch, no registry rebuild.
        let denied = run(&tool_held_clone.granted_roots());
        policy.grant_extra(&target).unwrap();
        let allowed = run(&tool_held_clone.granted_roots());
        let written = std::fs::read_to_string(&probe);
        let _ = std::fs::remove_dir_all(&target);

        assert!(
            !denied.status.success(),
            "write outside the roots must be denied first"
        );
        assert!(
            allowed.status.success(),
            "granted root must be writable: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_eq!(
            written.unwrap().trim(),
            "built",
            "the write landed in the granted directory"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_reads_the_workspace_spill_dir() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        // Behavioral pin for the spill design: overflow files live under the
        // WORKSPACE's `.deep-code/spill` precisely so a sandboxed `grep`/`tail`
        // can mine them. The read-deny sealing the HOME `~/.deep-code` secret
        // store must not catch a workspace directory of the same name.
        let workspace = tempfile::tempdir().unwrap();
        let spill = workspace.path().join(".deep-code/spill/run-1");
        std::fs::create_dir_all(&spill).unwrap();
        let log = spill.join("job_1.stdout.log");
        std::fs::write(&log, "first-error: E0308\n").unwrap();

        let output = wrap_shell_command(
            &format!("grep -c first-error {}", log.display()),
            workspace.path(),
            &single_root(workspace.path()),
            &SandboxPolicy::workspace_write(),
        )
        .output()
        .expect("sandbox-exec should launch");

        assert!(
            output.status.success(),
            "sandboxed read of a workspace spill file must pass: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_writes_into_extra_granted_root() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        // The multi-root scenario end to end at the kernel: a command run from
        // the workspace writes into the `--add-dir` root and Seatbelt allows
        // it. Before the grant existed this exact write was the user-visible
        // "Operation not permitted".
        let workspace = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let granted = vec![workspace.path().to_path_buf(), extra.path().to_path_buf()];
        let target = extra.path().join("from-sandbox.txt");

        let status = wrap_shell_command(
            &format!("printf granted > {}", target.display()),
            workspace.path(),
            &granted,
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandbox-exec should launch");

        assert!(
            status.success(),
            "write into a granted extra root must pass"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "granted");
    }

    /// Behavioral counterpart to `tmp_is_always_a_writable_root_on_unix`: the
    /// xcrun fallback path. Inside the sandbox `confstr` fails, so tools that
    /// cannot learn the real `$TMPDIR` write to `/tmp` — that write must pass,
    /// or every sandboxed `git` run carries EPERM noise that misclassifies
    /// its failures as write-boundary denials.
    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_can_write_to_slash_tmp() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let probe = format!("/tmp/.deep-code-tmp-probe-{}", std::process::id());

        let status = wrap_shell_command(
            &format!("printf tmp-ok > {probe} && rm -f {probe}"),
            workspace.path(),
            &single_root(workspace.path()),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandbox-exec should launch");

        let _ = std::fs::remove_file(&probe);
        assert!(status.success(), "a write to /tmp must pass in the sandbox");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_can_use_the_temp_dir() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();

        // Behavioral counterpart to `temp_dir_is_always_a_write_root`: without
        // the grant this fails with "Operation not permitted", which is exactly
        // how `cargo build`/`rustc`/`git` broke. `mktemp` needs no toolchain.
        let output = wrap_shell_command(
            "mktemp",
            workspace.path(),
            &single_root(workspace.path()),
            &SandboxPolicy::workspace_write(),
        )
        .output()
        .expect("sandbox-exec should launch");

        assert!(
            output.status.success(),
            "mktemp must work inside the sandbox; stderr was {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_writes_within_granted_root() {
        if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();

        let status = wrap_shell_command(
            "printf written-inside > inside.txt",
            workspace.path(),
            &single_root(workspace.path()),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandbox-exec should launch");

        assert!(status.success());
        let written = std::fs::read_to_string(workspace.path().join("inside.txt")).unwrap();
        assert_eq!(written, "written-inside");
    }
}
