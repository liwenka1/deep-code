//! OS sandbox helpers for shell execution.

mod policy;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos_seatbelt;
#[cfg(target_os = "windows")]
mod windows;

pub use policy::SandboxPolicy;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Model-facing note appended to a sandboxed command's output when its failure
/// looks like the OS sandbox denying a write. A boundary denial is the one
/// failure class no retry can fix — the kernel refuses regardless of the
/// command's spelling — so without this note the model reads a bare
/// "Operation not permitted" and reaches for sudo/chmod/path variants,
/// burning rounds on an outcome only the user can change.
///
/// This exact string doubles as the in-band marker the runtime uses to
/// classify the result as a boundary denial (circuit breaker + cascade
/// exemption); producer and consumer share the constant so they cannot drift.
pub(crate) const WRITE_DENIAL_NOTE: &str = "[note] this command ran inside the OS sandbox: writes \
are allowed only under the granted roots (the workspace and --add-dir directories). If this \
failure was a write outside them, retrying — including with sudo or chmod — cannot succeed. If \
the user intends that directory to be writable, ask them to grant it with the /add-dir command \
(or relaunch with --add-dir).";

/// Heuristic: does a failed *sandboxed* command's output look like the OS
/// denying a write? Matches the denial texts the two backends produce —
/// Seatbelt surfaces EPERM ("Operation not permitted"), Landlock EACCES
/// ("Permission denied") — plus EROFS for read-only remounts. Callers must
/// additionally know the command actually ran sandboxed and failed; this
/// function only inspects the text. Centralized here (not string-matched at
/// call sites) so the signature list has one home and one set of tests.
///
/// It is a heuristic: a plain EACCES on a root-owned file matches too. The
/// consumers are sized for that — the appended note is phrased as a
/// possibility, and the runtime's circuit breaker needs repeated hits before
/// it acts.
#[must_use]
pub(crate) fn write_denial_signature(exit_code: Option<i32>, stderr: &str) -> bool {
    if exit_code == Some(0) {
        return false;
    }
    [
        "Operation not permitted",
        "Permission denied",
        "Read-only file system",
    ]
    .iter()
    .any(|signature| stderr.contains(signature))
}

/// Detected sandbox backend for the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxBackend {
    #[default]
    None,
    #[cfg(target_os = "macos")]
    MacosSeatbelt,
    #[cfg(target_os = "linux")]
    LinuxLandlock,
    #[cfg(target_os = "windows")]
    WindowsJobObject,
}

impl SandboxBackend {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            #[cfg(target_os = "macos")]
            Self::MacosSeatbelt => "macos_seatbelt",
            #[cfg(target_os = "linux")]
            Self::LinuxLandlock => "linux_landlock",
            #[cfg(target_os = "windows")]
            Self::WindowsJobObject => "windows_job_object",
        }
    }
}

/// Platform sandbox capability report.
///
/// `available` only means "a backend exists"; it does NOT mean that backend
/// enforces anything in particular. The two `confines_*` flags say what it
/// actually does, and they are not the same on every platform: the Windows Job
/// Object contains a process *tree* (so cancel/timeout can kill it) but does not
/// restrict filesystem writes or network access at all. Anything that tells the
/// user — or the model — that a command is "sandboxed" or "offline" must consult
/// these rather than `available`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub backend: SandboxBackend,
    pub available: bool,
    /// Backend confines writes to the policy's writable roots.
    pub confines_filesystem: bool,
    /// Backend blocks network access unless the policy grants it.
    pub confines_network: bool,
    pub detail: String,
}

/// Whether this host's sandbox enforces BOTH confinement dimensions — i.e.
/// whether calling a command "sandboxed" to the user is truthful here.
#[must_use]
pub fn sandbox_confines_filesystem_and_network() -> bool {
    let caps = detect_capabilities();
    caps.confines_filesystem && caps.confines_network
}

/// Whether this host's sandbox actually withholds the network by default.
#[must_use]
pub fn sandbox_confines_network() -> bool {
    detect_capabilities().confines_network
}

/// Whether an OS sandbox backend is usable on this machine. For callers that
/// must refuse to run rather than silently degrade to bare execution (eval
/// blind-approves model commands on untrusted repos).
#[must_use]
pub fn sandbox_available() -> bool {
    detect_capabilities().available
}

/// Probe sandbox support on this host, memoized for the process lifetime.
///
/// Called several times per spawned command (`should_sandbox`,
/// `sandbox_unavailable_for`, `confine_spawned`), and on Linux each probe is a
/// real `landlock_create_ruleset` syscall plus an fd — so this must not re-probe
/// per call. Capability cannot change under a running process anyway.
#[must_use]
pub fn detect_capabilities() -> SandboxCapabilities {
    static CACHED: std::sync::OnceLock<SandboxCapabilities> = std::sync::OnceLock::new();
    CACHED.get_or_init(probe_capabilities).clone()
}

fn probe_capabilities() -> SandboxCapabilities {
    #[cfg(target_os = "macos")]
    {
        if macos_seatbelt::is_available() {
            return SandboxCapabilities {
                backend: SandboxBackend::MacosSeatbelt,
                available: true,
                confines_filesystem: true,
                confines_network: true,
                detail: "sandbox-exec (Seatbelt) is available".to_string(),
            };
        }
        SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            confines_filesystem: false,
            confines_network: false,
            detail: "sandbox-exec is missing or not permitted".to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    {
        linux::capabilities()
    }

    #[cfg(target_os = "windows")]
    {
        windows::capabilities()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            confines_filesystem: false,
            confines_network: false,
            detail: "unsupported platform".to_string(),
        }
    }
}

/// Keeps an OS sandbox alive for a spawned child's lifetime. On Windows it owns
/// the Job Object handle (dropping it kills the process tree); on macOS/Linux
/// it is empty, since those confine before spawn via [`SandboxManager::wrap_shell_command`].
#[derive(Debug)]
pub struct SandboxGuard {
    #[cfg(target_os = "windows")]
    _job: windows::JobGuard,
}

/// Prepares subprocess commands, optionally wrapping them in an OS sandbox.
#[derive(Debug, Clone, Default)]
pub struct SandboxManager {
    forced: Option<bool>,
}

impl SandboxManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force sandbox on/off for tests.
    #[cfg(test)]
    #[must_use]
    pub fn force_sandbox(mut self, enabled: Option<bool>) -> Self {
        self.forced = enabled;
        self
    }

    pub fn should_sandbox(&self, policy: &SandboxPolicy) -> bool {
        if !policy.should_sandbox() {
            return false;
        }
        if let Some(forced) = self.forced {
            return forced;
        }
        detect_capabilities().available
    }

    /// True when `policy` demands OS confinement but this host has no backend
    /// to provide it. Callers (shell/job spawn) refuse rather than run the
    /// command bare: the whole safety model leans on the sandbox being the
    /// real boundary, so a command the policy wanted confined must never
    /// silently escape to the host. A policy that asks for no sandbox
    /// ([`SandboxPolicy::Unsandboxed`]) runs bare by design; a test override
    /// ([`force_sandbox`](Self::force_sandbox)) is authoritative either way.
    #[must_use]
    pub fn sandbox_unavailable_for(&self, policy: &SandboxPolicy) -> bool {
        refuse_bare_execution(
            policy.should_sandbox(),
            self.forced,
            detect_capabilities().available,
        )
    }

    /// Build the confined command, or `Err(detail)` when confinement was wanted
    /// but could not be constructed.
    ///
    /// Failing here is not the same as having no backend (that is caught earlier
    /// by [`Self::sandbox_unavailable_for`]): the probe can pass and the
    /// per-command ruleset still fail to build. The old code logged a warning and
    /// returned the *bare* command, which contradicted the refuse-if-
    /// unenforceable policy — and the warning was invisible, because the TUI
    /// redirects stderr to a log file before any command runs.
    pub fn wrap_shell_command(
        &self,
        command: &str,
        cwd: &Path,
        granted_roots: &[PathBuf],
        policy: &SandboxPolicy,
    ) -> Result<Command, String> {
        if !self.should_sandbox(policy) {
            return Ok(bare_shell_command(command, cwd));
        }

        #[cfg(target_os = "macos")]
        {
            Ok(macos_seatbelt::wrap_shell_command(
                command,
                cwd,
                granted_roots,
                policy,
            ))
        }

        #[cfg(target_os = "linux")]
        {
            linux::wrap_shell_command(command, cwd, granted_roots, policy)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = granted_roots;
            Ok(bare_shell_command(command, cwd))
        }
    }

    /// Confine an already-spawned child where the OS sandbox must be applied
    /// post-spawn (Windows Job Object). Returns a guard to retain for the
    /// child's lifetime, or `None` when no post-spawn step is needed (macOS and
    /// Linux confine via [`Self::wrap_shell_command`]) or sandboxing is off.
    #[must_use]
    pub fn confine_spawned(
        &self,
        child: &tokio::process::Child,
        policy: &SandboxPolicy,
    ) -> Option<SandboxGuard> {
        if !self.should_sandbox(policy) {
            return None;
        }
        #[cfg(target_os = "windows")]
        {
            // `raw_handle()` is `None` once the child has already exited — the
            // same tiny fail-open window as the old pre-tokio path.
            Some(SandboxGuard {
                _job: windows::confine(child.raw_handle()?)?,
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = child;
            None
        }
    }
}

/// Pure decision for [`SandboxManager::sandbox_unavailable_for`], split out so
/// the truth table is unit-testable without a platform probe. Refuse only when
/// the policy wants a sandbox, no test override is in play, and the host has no
/// backend.
fn refuse_bare_execution(
    policy_wants_sandbox: bool,
    forced: Option<bool>,
    available: bool,
) -> bool {
    policy_wants_sandbox && forced.is_none() && !available
}

/// Build `cmd /C <command>` with the command line passed to `cmd.exe` verbatim.
///
/// `Command::arg` applies the MSVC C-runtime quoting rules: an argument holding
/// spaces or quotes is wrapped in `"` and its inner quotes escaped as `\"`.
/// `cmd.exe` implements none of that — it treats `\` as an ordinary character
/// and `"` as a quote toggle — so `git commit -m "msg"` reached the shell as
/// `git commit -m \"msg\"` and ran with literal backslashes in the message.
/// `raw_arg` exists for exactly this case.
///
/// Two consequences of the old behaviour, both real: every Windows command
/// carrying a quoted argument was silently corrupted (the observable being that
/// the model gave up on `git commit -m` and started writing the message to a
/// file to commit with `-F`), and what actually executed differed from the
/// command string shown in the approval panel — the user approved one thing and
/// `cmd` ran another.
#[cfg(windows)]
fn bare_shell_command(command: &str, cwd: &Path) -> Command {
    use std::os::windows::process::CommandExt;

    let mut cmd = Command::new("cmd");
    // `/C` is a bare token, so normal quoting is correct for it; only the
    // command itself must bypass the escaping. Arguments are still joined with
    // a space, so this yields `cmd /C <command>`.
    cmd.arg("/C");
    cmd.raw_arg(command);
    cmd.current_dir(cwd);
    cmd
}

#[cfg(not(windows))]
fn bare_shell_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(cwd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_capabilities_returns_structured_report() {
        let caps = detect_capabilities();
        assert!(!caps.detail.is_empty());
    }

    #[test]
    fn write_denial_signature_matches_backend_denial_texts() {
        // The three texts the backends actually produce: Seatbelt EPERM,
        // Landlock EACCES, and a read-only remount.
        assert!(write_denial_signature(
            Some(1),
            "sh: /other/repo/f.txt: Operation not permitted"
        ));
        assert!(write_denial_signature(
            Some(1),
            "touch: cannot touch '/other/repo/f.txt': Permission denied"
        ));
        assert!(write_denial_signature(Some(1), "Read-only file system"));
        // A killed child reports no exit code; the stderr text still decides.
        assert!(write_denial_signature(None, "Operation not permitted"));

        // A successful command is never a denial, whatever stderr says.
        assert!(!write_denial_signature(
            Some(0),
            "warning: Operation not permitted (ignored)"
        ));
        // Ordinary failures don't match.
        assert!(!write_denial_signature(Some(1), "error: expected `;`"));
        assert!(!write_denial_signature(Some(2), ""));
    }

    #[test]
    fn refuse_bare_execution_only_when_wanted_unforced_and_unavailable() {
        // The one refusing case: policy wants a sandbox, no test override, and
        // the host has no backend.
        assert!(refuse_bare_execution(true, None, false));
        // Backend present → run (confined).
        assert!(!refuse_bare_execution(true, None, true));
        // Policy doesn't want a sandbox → bare by design, never refuse.
        assert!(!refuse_bare_execution(false, None, false));
        // Test override is authoritative in both directions — never refuse.
        assert!(!refuse_bare_execution(true, Some(false), false));
        assert!(!refuse_bare_execution(true, Some(true), false));
    }

    /// Regression guard for Windows argument passing.
    ///
    /// This started life as a diagnostic probe and it caught a real defect:
    /// `bare_shell_command` used `Command::arg` for the whole command line, which
    /// applies the MSVC C-runtime quoting rules (an argument holding spaces or
    /// quotes is wrapped in `"`, inner quotes escaped as `\"`). `cmd.exe`
    /// implements none of that — `\` is an ordinary character and `"` a quote
    /// toggle — so every command carrying a quoted argument arrived mangled. The
    /// fix is `raw_arg`; this test fails loudly if anyone reverts to `arg`.
    ///
    /// `echo` is the right probe because cmd's `echo` emits its argument
    /// verbatim, quotes included — a correct pass-through prints exactly what was
    /// typed, and a stray backslash means the escaping leaked through.
    ///
    /// The observable behind the original report: on Windows the model stopped
    /// using `git commit -m "<message>"` and started writing the message to a file
    /// to commit with `-F`, i.e. it routed around the broken quoting.
    #[cfg(windows)]
    #[test]
    fn windows_cmd_receives_quoted_arguments_verbatim() {
        let cwd = std::env::current_dir().expect("cwd");
        let run = |command: &str| {
            let output = bare_shell_command(command, &cwd)
                .output()
                .expect("spawn cmd");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let got = run("echo \"a b\"");
        assert!(
            !got.contains('\\'),
            "cmd received escaped quotes: stdout={got:?}. `Command::arg` applies \
             MSVC quoting that cmd.exe cannot parse; use raw_arg."
        );
        assert_eq!(
            got, "\"a b\"",
            "quoted argument did not survive the trip to cmd.exe: stdout={got:?}"
        );

        // The shape from the real report: several quoted arguments in one command,
        // which is what `git commit -m "…"` looks like to cmd.
        let got = run("echo \"a b\" \"c d\"");
        assert_eq!(
            got, "\"a b\" \"c d\"",
            "multiple quoted arguments were mangled: stdout={got:?}"
        );
    }
}
