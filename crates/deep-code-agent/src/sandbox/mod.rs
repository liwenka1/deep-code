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

use serde::Serialize;

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

/// A confinement dimension this host enforces, but not completely.
///
/// Every gap here is a right the OS itself cannot express on this machine, not
/// a rule we chose to skip — the backend already requests the widest set the
/// kernel offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementGap {
    /// `truncate(2)`/`ftruncate(2)` on a path outside the writable roots is not
    /// refused: `LANDLOCK_ACCESS_FS_TRUNCATE` arrived in ABI 3 (Linux 6.2), and
    /// a right the kernel never *handles* is a right it never checks. Every
    /// other write outside the roots — create, delete, open-for-write — is
    /// still refused, so the exposure is destructive (a file can be emptied),
    /// never disclosing.
    LandlockTruncate,
    /// `ioctl(2)` on an already-opened character or block device is not
    /// refused: `LANDLOCK_ACCESS_FS_IOCTL_DEV` arrived in ABI 5 (Linux 6.10).
    ///
    /// This does NOT weaken the path-write boundary — an ordinary write, create,
    /// delete or truncate outside the roots is still refused wherever the
    /// corresponding right exists. What it exposes is the device itself: this
    /// backend leaves reads unhandled on purpose, so a device node the user can
    /// open at all is reachable read-only, and an `ioctl` on it is then
    /// unchecked. Where that device is a disk (root in a container, or a user in
    /// `disk`/`kvm`), `SG_IO` carries a raw write; where it is a terminal, it
    /// carries `TIOCSTI`. So the reach is bounded by which device nodes the
    /// invoking user can open — not by which paths the policy granted.
    LandlockIoctlDev,
}

impl EnforcementGap {
    /// One line naming what stays unenforced and the kernel that would fix it.
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::LandlockTruncate => {
                "truncate(2) outside the writable roots is not refused (needs Landlock ABI 3, Linux 6.2+)"
            }
            Self::LandlockIoctlDev => {
                "ioctl(2) on an opened device node is not refused (needs Landlock ABI 5, Linux 6.10+)"
            }
        }
    }

    /// Whether this gap weakens the *path*-write boundary — the promise that a
    /// write aimed at a path outside the granted roots is refused.
    ///
    /// The distinction is not academic: it decides what the model is told, and
    /// the two gaps differ. ABI 5 landed in Linux 6.10 (July 2024), so on every
    /// mainstream kernel below it — Ubuntu 24.04's 6.8 included — the only gap
    /// is [`Self::LandlockIoctlDev`]. Letting that one select a blanket "do not
    /// treat this boundary as a safety net" told the model something false on
    /// the majority of Linux hosts, and pushed it off a boundary that was fully
    /// intact. Understating enforcement is a cheaper mistake than overstating
    /// it, but it is still a wrong answer, and a warning every host shows is a
    /// warning nobody reads.
    #[must_use]
    pub fn weakens_path_writes(self) -> bool {
        match self {
            Self::LandlockTruncate => true,
            Self::LandlockIoctlDev => false,
        }
    }

    /// The sentence handed to the *model* for this gap. Distinct from
    /// [`Self::detail`], which is written for a human reading `doctor`: the
    /// model needs to know what to do differently, not which ABI is missing.
    #[must_use]
    pub fn model_caveat(self) -> &'static str {
        match self {
            Self::LandlockTruncate => {
                "This host's kernel does not enforce truncate(2) outside the granted roots, so a \
                 file outside them can still be emptied even though creating, deleting and opening \
                 it for writing are refused: do not aim a truncating or destructive command at a \
                 path outside the granted roots expecting the OS to refuse it."
            }
            Self::LandlockIoctlDev => {
                "This host's kernel does not govern ioctl(2) on device nodes. Ordinary writes \
                 outside the granted roots are still refused, so the write boundary holds; just do \
                 not drive devices under /dev directly."
            }
        }
    }
}

/// How completely a backend confines one dimension (writes, or network).
///
/// A bool cannot say "enforced, except for this", and that state is real:
/// Landlock confines writes on every kernel from 5.13, yet the right that
/// governs `truncate(2)` only exists from ABI 3 (Linux 6.2). Reporting the
/// older kernel as a plain `true` promises a boundary that does not hold —
/// which is the one thing a safety report must never do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Enforcement {
    /// Nothing in this dimension is confined.
    None,
    /// Confined apart from `gaps`, which this host cannot express.
    Partial { gaps: Vec<EnforcementGap> },
    /// Confined with no known gap.
    Full,
}

impl Enforcement {
    /// Build from the gaps a backend found: no gaps means [`Self::Full`].
    #[must_use]
    pub fn from_gaps(gaps: Vec<EnforcementGap>) -> Self {
        if gaps.is_empty() {
            Self::Full
        } else {
            Self::Partial { gaps }
        }
    }

    /// Whether this dimension is confined at all. Use for "is the network
    /// withheld by default"-style questions, where a partial answer still means
    /// the guarantee exists.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this dimension is confined with no known gap. Use before telling
    /// a human that a command is "sandboxed".
    #[must_use]
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    #[must_use]
    pub fn gaps(&self) -> &[EnforcementGap] {
        match self {
            Self::Partial { gaps } => gaps,
            Self::None | Self::Full => &[],
        }
    }

    /// The weaker of two dimensions, keeping every gap either one names — the
    /// most a single answer may claim about a host that must satisfy both.
    ///
    /// One unconfined dimension makes the whole answer [`Self::None`]: a report
    /// that says "unconfined" understates nothing, so the other dimension's
    /// gaps have nothing left to add there.
    ///
    /// Public because `doctor` needs the same answer the approval panel and the
    /// tool descriptions get. It used to re-derive it by hand, which is two
    /// definitions of "what does this host enforce overall" — and they had
    /// already drifted: the hand-rolled one printed `partial` for a Windows host
    /// that confines nothing.
    #[must_use]
    pub fn weakest(filesystem: Self, network: Self) -> Self {
        match (filesystem, network) {
            (Self::None, _) | (_, Self::None) => Self::None,
            // Both parameters are owned, so when one side is `Full` the other
            // side's allocation is already the answer — no rebuild.
            (Self::Full, other) | (other, Self::Full) => other,
            (filesystem, network) => {
                // Gap variants belong to one dimension each, so appending
                // cannot duplicate.
                let mut gaps = filesystem.gaps().to_vec();
                gaps.extend_from_slice(network.gaps());
                Self::Partial { gaps }
            }
        }
    }
}

/// Platform sandbox capability report.
///
/// `available` only means "a backend exists"; it does NOT mean that backend
/// enforces anything in particular. The two [`Enforcement`] fields say what it
/// actually does, and they are not the same on every platform: the Windows Job
/// Object contains a process *tree* (so cancel/timeout can kill it) but does not
/// restrict filesystem writes or network access at all. Anything that tells the
/// user — or the model — that a command is "sandboxed" or "offline" must consult
/// these rather than `available`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub backend: SandboxBackend,
    pub available: bool,
    /// How completely writes are confined to the policy's writable roots.
    pub filesystem: Enforcement,
    /// How completely network access is withheld unless the policy grants it.
    pub network: Enforcement,
    pub detail: String,
}

/// The weaker of this host's two confinement dimensions — the most any surface
/// may honestly claim about a command it is about to run — carrying the gaps of
/// both, so a partial answer can still name everything that is missing.
///
/// Borrowed and memoized, not returned by value: the approval panel calls this
/// while rendering, i.e. once per frame for as long as a prompt is on screen,
/// and every call used to clone the whole capability report (a `String` plus a
/// `Vec` per partial dimension) to read two discriminants.
#[must_use]
pub fn sandbox_enforcement() -> &'static Enforcement {
    static CACHED: std::sync::OnceLock<Enforcement> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| {
        let caps = detect_capabilities();
        Enforcement::weakest(caps.filesystem, caps.network)
    })
}

/// Model-facing sentences for refusals this sandbox imposes BY DESIGN — rights
/// the kernel could grant that the backend deliberately withholds.
///
/// The complement of [`EnforcementGap`]: a gap names what this host *cannot*
/// enforce, so no surface overclaims; a design note names what the sandbox
/// *chose* to refuse, so the refusal is read as intent rather than as a
/// write-boundary denial to be fixed with a path grant. Both exist for the
/// same reader — the model — because both failure shapes surface as
/// "Permission denied".
///
/// Empty on every platform but Linux: Seatbelt allows pty allocation and
/// device ioctl outright (see `macos_seatbelt.rs`), and a backend that
/// confines nothing has nothing it refuses on purpose.
#[must_use]
pub fn sandbox_design_notes() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        linux::design_notes()
    }
    #[cfg(not(target_os = "linux"))]
    {
        &[]
    }
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
            // The profile names its writable roots and its network rule
            // outright, so there is no kernel-version negotiation to degrade:
            // sandbox-exec either applies the profile or fails to launch.
            return SandboxCapabilities {
                backend: SandboxBackend::MacosSeatbelt,
                available: true,
                filesystem: Enforcement::Full,
                network: Enforcement::Full,
                detail: "sandbox-exec (Seatbelt) is available".to_string(),
            };
        }
        SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            filesystem: Enforcement::None,
            network: Enforcement::None,
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
            filesystem: Enforcement::None,
            network: Enforcement::None,
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
    fn no_gaps_is_full_and_any_gap_is_partial() {
        assert_eq!(Enforcement::from_gaps(Vec::new()), Enforcement::Full);
        assert_eq!(
            Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]),
            Enforcement::Partial {
                gaps: vec![EnforcementGap::LandlockTruncate],
            }
        );
    }

    #[test]
    fn partial_is_enforced_but_not_full() {
        // The distinction the whole report exists for: a partial dimension still
        // holds a boundary (so "is the network withheld" stays true), yet must
        // never be described to a human as "sandboxed".
        let partial = Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]);
        assert!(partial.is_enforced());
        assert!(!partial.is_full());
        assert!(!Enforcement::None.is_enforced());
        assert!(Enforcement::Full.is_full());
    }

    #[test]
    fn gaps_are_listed_only_for_partial() {
        assert!(Enforcement::None.gaps().is_empty());
        assert!(Enforcement::Full.gaps().is_empty());
        assert_eq!(
            Enforcement::from_gaps(vec![
                EnforcementGap::LandlockTruncate,
                EnforcementGap::LandlockIoctlDev,
            ])
            .gaps(),
            [
                EnforcementGap::LandlockTruncate,
                EnforcementGap::LandlockIoctlDev
            ]
        );
    }

    #[test]
    fn weakest_never_claims_more_than_either_dimension() {
        // Every combination, because the host running this test only ever
        // exercises one of them (macOS is Full/Full, Windows None/None).
        let truncate = Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]);
        let ioctl = Enforcement::from_gaps(vec![EnforcementGap::LandlockIoctlDev]);
        let levels = [Enforcement::None, truncate.clone(), Enforcement::Full];

        for filesystem in &levels {
            for network in &levels {
                let weakest = Enforcement::weakest(filesystem.clone(), network.clone());
                assert_eq!(
                    weakest.is_full(),
                    filesystem.is_full() && network.is_full(),
                    "{filesystem:?} + {network:?}"
                );
                assert_eq!(
                    weakest.is_enforced(),
                    filesystem.is_enforced() && network.is_enforced(),
                    "{filesystem:?} + {network:?}"
                );
            }
        }

        // Two partial dimensions: the answer names both gaps rather than
        // silently reporting one dimension's and dropping the other's.
        assert_eq!(
            Enforcement::weakest(truncate, ioctl),
            Enforcement::Partial {
                gaps: vec![
                    EnforcementGap::LandlockTruncate,
                    EnforcementGap::LandlockIoctlDev
                ],
            }
        );
    }

    #[test]
    fn every_gap_explains_itself() {
        // The gap list is what `doctor` prints and what the READMEs promise is
        // nameable, so an empty or duplicated line would be a silent regression.
        for gap in [
            EnforcementGap::LandlockTruncate,
            EnforcementGap::LandlockIoctlDev,
        ] {
            assert!(!gap.detail().is_empty());
        }
        assert_ne!(
            EnforcementGap::LandlockTruncate.detail(),
            EnforcementGap::LandlockIoctlDev.detail()
        );
    }

    #[test]
    fn sandbox_enforcement_reports_the_weaker_dimension() {
        // Linux before 6.2 is the real shape: writes partial, network full. The
        // approval panel must show the write answer, not the network one.
        let caps = detect_capabilities();
        let weaker = sandbox_enforcement();
        assert_eq!(
            weaker.is_full(),
            caps.filesystem.is_full() && caps.network.is_full()
        );
        assert_eq!(
            weaker.is_enforced(),
            caps.filesystem.is_enforced() && caps.network.is_enforced()
        );
        // A `None` summary has nothing left to qualify (see `weakest`); anything
        // else must carry every gap either dimension named.
        if weaker.is_enforced() {
            for gap in caps.filesystem.gaps().iter().chain(caps.network.gaps()) {
                assert!(
                    weaker.gaps().contains(gap),
                    "{gap:?} was reported by a dimension but dropped from the summary"
                );
            }
        }
    }

    #[test]
    fn design_notes_and_ioctl_gap_are_mutually_exclusive() {
        // A design note says "the sandbox refuses device ioctl"; the gap says
        // "the kernel cannot check it". A description carrying both would tell
        // the model the same operation is simultaneously unchecked and refused.
        // And a host with no backend refuses nothing *by design* — there is no
        // design there to speak for.
        let caps = detect_capabilities();
        let notes = sandbox_design_notes();
        if !caps.available {
            assert!(
                notes.is_empty(),
                "an unavailable backend cannot refuse anything by design"
            );
        }
        if caps
            .filesystem
            .gaps()
            .contains(&EnforcementGap::LandlockIoctlDev)
        {
            assert!(
                notes.is_empty(),
                "device ioctl cannot be both ungoverned and deliberately denied"
            );
        }
        for note in notes {
            assert!(!note.is_empty());
        }
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
