//! Linux sandbox: Landlock filesystem confinement + seccomp syscall/network
//! filtering, applied in the forked child via `pre_exec` (no external binary,
//! unlike the macOS `sandbox-exec` wrapper).
//!
//! Model (parity with the macOS seatbelt backend):
//! - **Reads stay broad** — only write-class Landlock access rights are
//!   *handled*, so reads are unrestricted (keeps tools working).
//! - **Writes are confined** to the policy's writable roots, plus the temp dir
//!   and a few `/dev` nodes needed for redirections.
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
use super::{SandboxBackend, SandboxCapabilities};

/// Landlock ABI baseline (kernel 5.13+). `BestEffort` silently downgrades any
/// newer write-class rights on older kernels.
const LANDLOCK_ABI: ABI = ABI::V1;

/// Always-blocked syscalls (escape / privilege / host-tampering). Limited to
/// numbers present on both x86_64 and aarch64.
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
];

/// Blocked when the policy forbids network. Denying `socket` stops any new
/// socket (no network of any family); `connect` is belt-and-suspenders.
const NETWORK_SYSCALLS: &[i64] = &[libc::SYS_socket, libc::SYS_connect];

/// Write-class access rights: everything Landlock can govern minus the
/// read/execute rights, so reads stay unrestricted.
fn write_access() -> BitFlags<AccessFs> {
    AccessFs::from_all(LANDLOCK_ABI) & !AccessFs::from_read(LANDLOCK_ABI)
}

#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    match probe() {
        Ok(()) => SandboxCapabilities {
            backend: SandboxBackend::LinuxLandlock,
            available: true,
            detail: "Landlock + seccomp available".to_string(),
        },
        Err(error) => SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            detail: format!("Landlock unavailable: {error}"),
        },
    }
}

/// Truthful availability probe: `HardRequirement` makes ruleset creation error
/// when the kernel lacks Landlock, instead of silently no-opping.
fn probe() -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write_access())
        .and_then(|ruleset| ruleset.create())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub fn wrap_shell_command(
    command: &str,
    cwd: &Path,
    workspace: &Path,
    policy: &SandboxPolicy,
) -> Result<Command, String> {
    let mut cmd = super::bare_shell_command(command, cwd);

    // Fail closed. Previously this warned to stderr and returned the unconfined
    // command, which both contradicted the refuse-if-unenforceable policy and was
    // silent in practice (the TUI redirects stderr into a log file).
    let (ruleset, bpf) = build_restrictions(workspace, cwd, policy)?;
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
    workspace: &Path,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<(RulesetCreated, BpfProgram), String> {
    let ruleset = build_landlock(workspace, cwd, policy)?;
    let bpf = build_seccomp(policy)?;
    Ok((ruleset, bpf))
}

fn build_landlock(
    workspace: &Path,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<RulesetCreated, String> {
    let access = write_access();
    // Pre-filter to existing paths so a missing /dev node can't fail the whole
    // ruleset build (path_beneath_rules errors on paths it can't open).
    let writable: Vec<PathBuf> = writable_paths(workspace, cwd, policy)
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

fn writable_paths(workspace: &Path, cwd: &Path, policy: &SandboxPolicy) -> Vec<PathBuf> {
    // `writable_roots` already includes the temp dir (shared with the macOS
    // profile so the two backends cannot diverge on it again).
    let mut paths = policy.writable_roots(workspace, cwd);
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

fn build_seccomp(policy: &SandboxPolicy) -> Result<BpfProgram, String> {
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for syscall in DANGEROUS_SYSCALLS {
        rules.insert(*syscall, Vec::new());
    }
    if !policy.has_network_access() {
        for syscall in NETWORK_SYSCALLS {
            rules.insert(*syscall, Vec::new());
        }
    }

    let filter = SeccompFilter::new(
        rules,
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
            workspace,
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
