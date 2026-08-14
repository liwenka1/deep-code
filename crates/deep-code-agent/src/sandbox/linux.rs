//! Linux sandbox: Landlock filesystem confinement + seccomp syscall/network
//! filtering, applied in the forked child via `pre_exec` (no external binary,
//! unlike the macOS `sandbox-exec` wrapper).
//!
//! Model (parity with the macOS seatbelt backend):
//! - **Reads stay broad** — only write-class Landlock access rights are
//!   *handled*, so reads are unrestricted (keeps tools working).
//! - **Writes are confined** to the policy's writable roots, plus the temp dir
//!   and a few `/dev` nodes needed for redirections. How *completely* depends on
//!   the kernel: Landlock gained the right governing `truncate(2)` in ABI 3
//!   (Linux 6.2) and the one governing device `ioctl(2)` in ABI 5 (Linux 6.10),
//!   and a right the kernel cannot handle is a right it never checks. Older
//!   kernels are reported as [`Enforcement::Partial`] rather than confined, so
//!   no surface claims a boundary the host is not holding.
//! - **Network is blocked** (seccomp `socket`/`connect` → EPERM) unless the
//!   policy allows it. When the policy DOES allow network (approved/trusted
//!   writable commands), the broad reads above become an exfiltration surface:
//!   Landlock is allow-list only, so it cannot express "read everything except
//!   `~/.ssh`", and unlike the macOS backend we can't seal individual
//!   credential dirs against reads here. `~/.deep-code` is already unwritable
//!   (it is not among the writable roots), but its read exposure — and that of
//!   `~/.ssh`/`~/.aws` — is an accepted, documented residual risk on Linux.
//! - **Dangerous syscalls** (ptrace, mount, module load, bpf, …) are always
//!   blocked.
//!
//! Availability is reported by [`capabilities`]; the manager only calls
//! [`wrap_shell_command`] when Landlock is available. If the per-command ruleset
//! still fails to build, that is an error — the command is refused, never run
//! unconfined (fail-closed, matching the spawn-site guard).

use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, path_beneath_rules,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

use super::policy::SandboxPolicy;
use super::{Enforcement, EnforcementGap, SandboxBackend, SandboxCapabilities};

/// ABI used to enumerate write-class rights for the enforced ruleset. Applied
/// with `BestEffort` (see [`build_restrictions`]), so a kernel that lacks a
/// newer right downgrades instead of failing — naming the newest ABI therefore
/// costs nothing on kernel 5.13 and gains every right above it. The downgrade
/// is silent to the *ruleset*, which is why [`landlock_gaps`] reports it to the
/// user instead of letting [`capabilities`] claim a boundary the kernel is not
/// holding.
///
/// This was pinned to `V1`, which left a real hole: a right that is never
/// *handled* is never checked, and `LANDLOCK_ACCESS_FS_TRUNCATE` only exists
/// from ABI 3 — so `truncate -s 0 ~/.ssh/id_rsa` destroyed a file outside every
/// writable root while the user believed the workspace boundary held.
///
/// `V5` and not the crate's newest (`V7`) on purpose: it is the last ABI that
/// introduces a new *`AccessFs`* right (`REFER` in 2, `TRUNCATE` in 3,
/// `IOCTL_DEV` in 5). 6 and 7 only add scopes, which this backend does not use.
const LANDLOCK_ABI: ABI = ABI::V5;

/// Minimum ABI that means "Landlock exists and will enforce" (kernel 5.13+).
///
/// Kept separate from [`LANDLOCK_ABI`] and deliberately NOT raised: [`probe`]
/// asserts this level as a `HardRequirement`, so requiring a newer ABI here
/// would report the sandbox unavailable on every kernel below it — and since
/// unavailable means refuse-to-run, that would reject every shell command on
/// virtually every Linux host.
const PROBE_ABI: ABI = ABI::V1;

/// The ABI levels that each add a write-class `AccessFs` right, paired with the
/// gap left when this kernel predates them.
///
/// `REFER` (ABI 2) is deliberately absent: Landlock denies cross-directory link
/// and rename by default on every kernel that lacks it, so its absence is
/// stricter than its presence, not a hole. ABI 6 and 7 add only scopes, which
/// this backend does not handle.
const WRITE_RIGHT_LEVELS: &[(ABI, EnforcementGap)] = &[
    (ABI::V3, EnforcementGap::LandlockTruncate),
    (ABI::V5, EnforcementGap::LandlockIoctlDev),
];

/// Always-blocked syscalls (escape / privilege / host-tampering / filter
/// bypass). Limited to numbers present on both x86_64 and aarch64.
const DANGEROUS_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_bpf,
    // io_uring is a second way to submit the work a syscall would do, and
    // seccomp only sees syscalls: `IORING_OP_SOCKET` + `IORING_OP_CONNECT`
    // open a connection without ever calling `socket(2)` or `connect(2)`, so
    // leaving the ring reachable would make `NETWORK_SYSCALLS` advisory
    // rather than enforced — and this backend reports the network dimension as
    // fully enforced. Blocked unconditionally, not only under a no-network
    // policy, so the filter means the same thing whatever the policy says.
    // `io_uring_setup` returning EPERM is a case every user of the interface
    // already handles (it is how they cope with older kernels): they fall back
    // to epoll/blocking I/O.
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Blocked when the policy forbids network. Denying `socket` stops any new
/// socket (no network of any family); `connect` is belt-and-suspenders. The
/// interface that could sidestep both — io_uring — is denied above, on every
/// policy.
const NETWORK_SYSCALLS: &[i64] = &[libc::SYS_socket, libc::SYS_connect];

/// Write-class access rights: everything Landlock can govern minus the
/// read/execute rights, so reads stay unrestricted.
fn write_access(abi: ABI) -> BitFlags<AccessFs> {
    AccessFs::from_all(abi) & !AccessFs::from_read(abi)
}

#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    match probe() {
        Ok(()) => {
            let filesystem = Enforcement::from_gaps(landlock_gaps());
            // A count, not the sentences: every gap is enumerable through
            // `gaps()`, and `doctor` prints one line each from there. Spelling
            // them out here too put the same text on screen twice.
            let detail = if filesystem.is_full() {
                "Landlock + seccomp available".to_string()
            } else {
                format!(
                    "Landlock + seccomp available; {} write-class right(s) this kernel cannot enforce",
                    filesystem.gaps().len()
                )
            };
            SandboxCapabilities {
                backend: SandboxBackend::LinuxLandlock,
                available: true,
                filesystem,
                // Full, and unlike Landlock not negotiated per kernel: the
                // filter either loads or the command is refused. That is a
                // claim about the filter *loading*, so it only holds while
                // nothing can submit network work behind seccomp's back —
                // which is why io_uring is in `DANGEROUS_SYSCALLS`.
                network: Enforcement::Full,
                detail,
            }
        }
        Err(error) => SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            filesystem: Enforcement::None,
            network: Enforcement::None,
            detail: format!("Landlock unavailable: {error}"),
        },
    }
}

/// Truthful availability probe: `HardRequirement` makes ruleset creation error
/// when the kernel lacks Landlock, instead of silently no-opping.
fn probe() -> Result<(), String> {
    probe_at(PROBE_ABI)
}

/// Whether this kernel enforces every write-class right `abi` defines.
///
/// `HardRequirement` is what makes this an answer rather than a guess: it turns
/// an unsupported right into a ruleset-creation error instead of a silent
/// downgrade.
fn probe_at(abi: ABI) -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write_access(abi))
        .and_then(|ruleset| ruleset.create())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Which write-class rights this kernel cannot enforce.
///
/// The `landlock` crate exposes no runtime ABI query on purpose — it documents
/// that choosing an ABI from the running kernel makes sandbox behavior
/// non-deterministic — so ask the kernel the only way it answers: try to create
/// a `HardRequirement` ruleset at each level that adds a right, and collect the
/// ones it refuses. At most two extra `landlock_create_ruleset` syscalls, made
/// once per process behind [`super::detect_capabilities`]'s memo.
///
/// Any error counts as a gap, including a non-compat one (ENOMEM, EMFILE) that
/// has nothing to do with the ABI. That is deliberate and barely reachable:
/// [`probe`] has already created a ruleset at ABI 1 by the time this runs, and
/// mistaking a fluke for a gap can only make the report understate what the
/// host enforces — never overstate it, which is the only direction that would
/// matter.
fn landlock_gaps() -> Vec<EnforcementGap> {
    WRITE_RIGHT_LEVELS
        .iter()
        .filter(|(abi, _)| probe_at(*abi).is_err())
        .map(|(_, gap)| *gap)
        .collect()
}

pub fn wrap_shell_command(
    command: &str,
    cwd: &Path,
    granted_roots: &[PathBuf],
    policy: &SandboxPolicy,
) -> Result<Command, String> {
    let mut cmd = super::bare_shell_command(command, cwd);

    // Fail closed. Previously this warned to stderr and returned the unconfined
    // command, which both contradicted the refuse-if-unenforceable policy and was
    // silent in practice (the TUI redirects stderr into a log file).
    let (ruleset, bpf) = build_restrictions(granted_roots, cwd, policy)?;
    let mut ruleset = Some(ruleset);

    // SAFETY: the closure runs in the forked child after fork and before exec.
    // The Landlock ruleset and seccomp program are fully built before the fork;
    // here we only invoke the enforcement syscalls (`restrict_self` /
    // `apply_filter`), which is acceptable in the post-fork pre-exec window.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(created) = ruleset.take() {
                created
                    .restrict_self()
                    .map_err(|error| io::Error::other(format!("landlock: {error}")))?;
            }
            seccompiler::apply_filter(&bpf)
                .map_err(|error| io::Error::other(format!("seccomp: {error}")))?;
            Ok(())
        });
    }
    Ok(cmd)
}

fn build_restrictions(
    granted_roots: &[PathBuf],
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<(RulesetCreated, BpfProgram), String> {
    let ruleset = build_landlock(granted_roots, cwd, policy)?;
    let bpf = build_seccomp(policy)?;
    Ok((ruleset, bpf))
}

fn build_landlock(
    granted_roots: &[PathBuf],
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<RulesetCreated, String> {
    let access = write_access(LANDLOCK_ABI);
    // Pre-filter to existing paths so a missing /dev node can't fail the whole
    // ruleset build (path_beneath_rules errors on paths it can't open).
    let writable: Vec<PathBuf> = writable_paths(granted_roots, cwd, policy)
        .into_iter()
        .filter(|path| path.exists())
        .collect();

    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access)
        .and_then(|ruleset| ruleset.create())
        .and_then(|created| created.add_rules(path_beneath_rules(&writable, access)))
        .map_err(|error| format!("landlock ruleset: {error}"))
}

fn writable_paths(granted_roots: &[PathBuf], cwd: &Path, policy: &SandboxPolicy) -> Vec<PathBuf> {
    // `writable_roots` already includes the temp dir (shared with the macOS
    // profile so the two backends cannot diverge on it again).
    let mut paths = policy.writable_roots(granted_roots, cwd);
    for node in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
        "/dev/random",
        "/dev/urandom",
    ] {
        paths.push(PathBuf::from(node));
    }
    paths
}

/// Syscalls this policy denies. An empty rule vector means "no argument
/// condition": every call of that number takes the filter's match action.
///
/// Split out from [`build_seccomp`] because the compiled BPF is opaque, and
/// "which syscalls does a given policy actually deny" is the property worth
/// testing.
fn seccomp_rules(policy: &SandboxPolicy) -> BTreeMap<i64, Vec<seccompiler::SeccompRule>> {
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for syscall in DANGEROUS_SYSCALLS {
        rules.insert(*syscall, Vec::new());
    }
    if !policy.has_network_access() {
        for syscall in NETWORK_SYSCALLS {
            rules.insert(*syscall, Vec::new());
        }
    }
    rules
}

fn build_seccomp(policy: &SandboxPolicy) -> Result<BpfProgram, String> {
    let filter = SeccompFilter::new(
        seccomp_rules(policy),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )
    .map_err(|error| format!("seccomp filter: {error}"))?;

    filter
        .try_into()
        .map_err(|error| format!("seccomp compile: {error}"))
}

fn target_arch() -> Result<TargetArch, String> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err("unsupported architecture for seccomp".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn run(workspace: &Path, command: &str) -> std::process::Output {
        wrap_shell_command(
            command,
            workspace,
            &[workspace.to_path_buf()],
            &SandboxPolicy::workspace_write(),
        )
        .expect("landlock ruleset should build on a host that reports it available")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn sandboxed command")
    }

    #[test]
    fn io_uring_is_denied_under_every_policy() {
        // The ring is a second way to submit the work a syscall would do, and
        // seccomp only sees syscalls — an `IORING_OP_SOCKET`/`IORING_OP_CONNECT`
        // pair reaches the network without `socket(2)` or `connect(2)`. So this
        // is what lets `capabilities()` report the network dimension as `Full`
        // rather than `Partial`: if the ring were reachable, that report would
        // be the same kind of overclaim the filesystem gaps exist to avoid.
        // Denied on the network-granted policy too, so the guarantee does not
        // depend on which policy happens to be in force.
        for policy in [
            SandboxPolicy::workspace_write(),
            SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
        ] {
            let rules = seccomp_rules(&policy);
            for syscall in [
                libc::SYS_io_uring_setup,
                libc::SYS_io_uring_enter,
                libc::SYS_io_uring_register,
            ] {
                assert!(
                    rules.contains_key(&syscall),
                    "syscall {syscall} must be denied under {policy:?}"
                );
            }
        }
    }

    #[test]
    fn network_syscalls_are_denied_only_without_a_network_grant() {
        let denied = seccomp_rules(&SandboxPolicy::workspace_write());
        let granted = seccomp_rules(&SandboxPolicy::WorkspaceWrite {
            network_access: true,
        });
        for syscall in NETWORK_SYSCALLS {
            assert!(denied.contains_key(syscall));
            assert!(!granted.contains_key(syscall));
        }
    }

    #[test]
    fn write_inside_workspace_is_allowed() {
        if !capabilities().available {
            return; // No Landlock on this host (e.g. old kernel); nothing to assert.
        }
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), "echo hi > inside.txt");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join("inside.txt").exists());
    }

    #[test]
    fn write_outside_workspace_is_blocked() {
        if !capabilities().available {
            return;
        }
        // Target a path under $HOME (not the workspace, not the granted temp
        // dir), so only Landlock decides the outcome.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let escape = PathBuf::from(&home).join("deep-code_sandbox_escape_probe.txt");
        let _ = std::fs::remove_file(&escape);

        let dir = tempfile::tempdir().unwrap();
        let _ = run(dir.path(), &format!("echo escaped > {}", escape.display()));

        let leaked = escape.exists();
        let _ = std::fs::remove_file(&escape);
        assert!(
            !leaked,
            "write outside workspace should be blocked by Landlock"
        );
    }
}
