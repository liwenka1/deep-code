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
/// profile derived from `policy`, with `workspace` (and `cwd`, when distinct)
/// as the writable roots.
pub fn wrap_shell_command(
    command: &str,
    cwd: &Path,
    workspace: &Path,
    policy: &SandboxPolicy,
) -> Command {
    let profile = compose_profile(policy, workspace, cwd);

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
fn compose_profile(policy: &SandboxPolicy, workspace: &Path, cwd: &Path) -> SeatbeltProfile {
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

    // Grant writes only under the roots the policy hands out (workspace and,
    // when different, the command's cwd). Paths are canonicalized because
    // Seatbelt matches the real path — on macOS /tmp is a symlink into
    // /private, and an uncanonicalized grant there would never match.
    let mut granted: Vec<PathBuf> = Vec::new();
    for root in policy.writable_roots(workspace, cwd) {
        let resolved = root.canonicalize().unwrap_or(root);
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

    // deep-code's OWN config dir holds the plaintext API key. No sandboxed
    // subprocess ever needs to touch it, so deny BOTH write and read — this is
    // the one credential store we can fully seal without breaking legitimate
    // tools, and it closes the highest-value target: a single approved network
    // command reading the key off disk, or rewriting `provider.base_url` to a
    // proxy. Read-deny is rendered after `(allow file-read*)` so it wins.
    if let Some(home) = crate::paths::home_dir() {
        let param = profile.bind_path("KEEP_DEEP_CODE", home.join(".deep-code"));
        profile.rule(format!("(deny file-write* (subpath {param}))"));
        profile.rule(format!("(deny file-read* (subpath {param}))"));
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
fn credential_dirs() -> Vec<(String, PathBuf)> {
    let Some(home) = crate::paths::home_dir() else {
        return Vec::new();
    };
    [
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
    ]
    .into_iter()
    .map(|entry| (credential_param_name(entry), home.join(entry)))
    .collect()
}

/// A valid SBPL `-D` parameter name for a credential entry: `KEEP_` plus the
/// entry with every non-alphanumeric char folded to `_` (so a subpath like
/// `.config/gh` or a file like `.git-credentials` yields a legal identifier).
fn credential_param_name(entry: &str) -> String {
    let body: String = entry
        .trim_start_matches('.')
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("KEEP_{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_probe_is_stable_and_panic_free() {
        // Two calls must agree (the verdict is cached process-wide).
        assert_eq!(is_available(), is_available());
    }

    #[test]
    fn profile_opens_with_deny_by_default() {
        let ws = Path::new("/tmp/dc-ws");
        let text = compose_profile(&SandboxPolicy::workspace_write(), ws, ws).render();
        assert!(text.starts_with("(version 1)\n(deny default)"));
        assert!(text.contains("(allow file-read*)"));
    }

    #[test]
    fn network_rules_appear_only_when_policy_grants_network() {
        let ws = Path::new("/tmp/dc-ws");
        let offline = compose_profile(&SandboxPolicy::workspace_write(), ws, ws).render();
        assert!(!offline.contains("network-outbound"));

        let online = compose_profile(
            &SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
            ws,
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
            workspace.path(),
            elsewhere.path(),
        );
        let write_roots: Vec<&str> = profile
            .bindings
            .iter()
            .filter(|(name, _)| name.starts_with("WRITE_ROOT_"))
            .map(|(name, _)| name.as_str())
            .collect();
        // Workspace, the distinct cwd, and the temp dir — see
        // `SandboxPolicy::writable_roots` for why the temp dir is mandatory.
        assert_eq!(
            write_roots,
            ["WRITE_ROOT_0", "WRITE_ROOT_1", "WRITE_ROOT_2"]
        );
        assert!(profile.render().contains("WRITE_ROOT_1"));
    }

    #[test]
    fn temp_dir_is_always_a_write_root() {
        // Regression guard: `rustc`, `mktemp` and git's xcrun shim all write to
        // $TMPDIR unconditionally, so dropping this grant does not confine
        // `cargo build`, it breaks it. Both backends read `writable_roots`.
        let workspace = tempfile::tempdir().unwrap();
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            workspace.path(),
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
            home.as_ref(),
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
            Path::new("/tmp/dc-ws"),
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
    fn credential_param_names_are_legal_identifiers() {
        // Subpath / dotted-file entries must fold to alnum+underscore names, or
        // sandbox-exec rejects the `-D` binding.
        assert_eq!(credential_param_name(".config/gh"), "KEEP_CONFIG_GH");
        assert_eq!(
            credential_param_name(".git-credentials"),
            "KEEP_GIT_CREDENTIALS"
        );
        assert_eq!(credential_param_name(".ssh"), "KEEP_SSH");
    }

    #[test]
    fn every_referenced_param_is_bound() {
        // sandbox-exec rejects profiles referencing unbound params; make sure
        // rules and bindings cannot drift apart.
        let workspace = tempfile::tempdir().unwrap();
        let profile = compose_profile(
            &SandboxPolicy::workspace_write(),
            workspace.path(),
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
        if !is_available() {
            eprintln!("seatbelt unavailable on this host; skipping");
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
            workspace.path(),
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

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_can_use_the_temp_dir() {
        if !is_available() {
            eprintln!("seatbelt unavailable on this host; skipping");
            return;
        }
        let workspace = tempfile::tempdir().unwrap();

        // Behavioral counterpart to `temp_dir_is_always_a_write_root`: without
        // the grant this fails with "Operation not permitted", which is exactly
        // how `cargo build`/`rustc`/`git` broke. `mktemp` needs no toolchain.
        let output = wrap_shell_command(
            "mktemp",
            workspace.path(),
            workspace.path(),
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
        if !is_available() {
            eprintln!("seatbelt unavailable on this host; skipping");
            return;
        }
        let workspace = tempfile::tempdir().unwrap();

        let status = wrap_shell_command(
            "printf written-inside > inside.txt",
            workspace.path(),
            workspace.path(),
            &SandboxPolicy::workspace_write(),
        )
        .status()
        .expect("sandbox-exec should launch");

        assert!(status.success());
        let written = std::fs::read_to_string(workspace.path().join("inside.txt")).unwrap();
        assert_eq!(written, "written-inside");
    }
}
