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
//!   and a right the kernel cannot handle is a right it never checks. Kernels
//!   missing either are reported as [`Enforcement::Partial`] rather than
//!   confined, so no surface claims a boundary the host is not holding — and
//!   because the two gaps are not the same gap, each carries its own wording
//!   rather than one blanket "this boundary is not a safety net" (see
//!   [`EnforcementGap`]).
//! - **Network is blocked** (seccomp `socket`/`connect` → EPERM) unless the
//!   policy allows it. When the policy DOES allow network (approved/trusted
//!   writable commands), the broad reads above become an exfiltration surface:
//!   Landlock is allow-list only, so it cannot express "read everything except
//!   `~/.ssh`", and unlike the macOS backend we can't seal individual
//!   credential dirs against reads here. `~/.deep-code` is already unwritable
//!   (it is not among the writable roots), but its read exposure — and that of
//!   `~/.ssh`/`~/.aws` — is an accepted, documented residual risk on Linux.
//!   The `/dev` nodes are granted write *without* `IOCTL_DEV`: redirection needs
//!   the write bit, and the ioctl bit reaches `TIOCSTI`.
//! - **Dangerous syscalls** are always blocked — the debugger family (`ptrace`
//!   and its no-`ptrace(2)` twins `process_vm_readv`/`pidfd_getfd`), both mount
//!   APIs, namespace entry AND creation (`unshare`/`setns` outright,
//!   `clone(CLONE_NEWUSER)` by argument filter), module load, `bpf`, … — plus
//!   an ENOSYS-answered set, `io_uring` and `clone3`, where EPERM would be
//!   mistaken for a boundary denial or break every libc's fallback path.
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
    RulesetCreatedAttr, RulesetError, path_beneath_rules,
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

/// Always-blocked syscalls (escape / privilege / host-tampering), denied with
/// EPERM. Limited to numbers present on both x86_64 and aarch64.
///
/// Grouped by the escape each entry closes, because the list is a denylist over
/// an `Allow` default: an entry that is merely *plausible* is cheap, and a
/// missing one is silent. Every syscall here is one a coding agent has no
/// legitimate use for, so the false-positive cost is near zero.
const DANGEROUS_SYSCALLS: &[i64] = &[
    // Debugger attach: reads and writes another process's memory. The child
    // runs as the SAME uid as the agent, whose heap holds the API key and whose
    // fds include live TLS sockets to the model endpoint.
    libc::SYS_ptrace,
    // `ptrace`'s lighter twins: same PTRACE_MODE_ATTACH permission check, no
    // `ptrace(2)` call. Blocking `ptrace` alone left the primitive reachable —
    // `process_vm_readv(getppid(), …)` lifts the key straight out of the parent,
    // and `pidfd_getfd` steals an already-connected socket, which would reach
    // the network with `socket`/`connect` denied and the ring shut.
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    libc::SYS_pidfd_open,
    // Mount manipulation. `mount(2)` is the classic spelling; the file-descriptor
    // mount API added in 5.2 does the same job without ever calling it, so
    // blocking only `mount` left the whole operation reachable by its new name.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    // Namespace escape/entry. `unshare(CLONE_NEWUSER)` hands the child
    // capabilities in a fresh namespace; `setns` walks into an existing one.
    // The third spelling — *creating* the namespace at process creation — is
    // closed elsewhere, because neither fits an unconditional list: `clone`
    // carries an argument filter in [`seccomp_rules`] (denying it outright
    // would deny fork itself), and `clone3` is answered ENOSYS in
    // [`enosys_rules`] (its flags live in a struct seccomp cannot read, and
    // ENOSYS is the one errno every libc answers by falling back to `clone`).
    libc::SYS_unshare,
    libc::SYS_setns,
    // Filesystem-handle reopen: resolves a path by opaque handle, sidestepping
    // the directory traversal Landlock reasons about.
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
    // Kernel introspection / side channels / host tampering.
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_bpf,
];

/// io_uring, denied with ENOSYS rather than EPERM (see [`build_seccomp`]).
///
/// The ring is a second way to submit the work a syscall would do, and seccomp
/// only sees syscalls: `IORING_OP_SOCKET` + `IORING_OP_CONNECT` open a
/// connection without ever calling `socket(2)` or `connect(2)`, so leaving it
/// reachable would make [`NETWORK_SYSCALLS`] advisory rather than enforced —
/// and this backend reports the network dimension as fully enforced. Denied
/// under every policy, so the filter means the same thing whatever the policy
/// says.
const IO_URING_SYSCALLS: &[i64] = &[
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Blocked when the policy forbids network. Denying `socket` stops any new
/// socket (no network of any family); `connect` is belt-and-suspenders. The
/// interface that could sidestep both — io_uring — is denied separately, on
/// every policy.
const NETWORK_SYSCALLS: &[i64] = &[libc::SYS_socket, libc::SYS_connect];

/// Write-class access rights: everything Landlock can govern minus the
/// read/execute rights, so reads stay unrestricted.
fn write_access(abi: ABI) -> BitFlags<AccessFs> {
    AccessFs::from_all(abi) & !AccessFs::from_read(abi)
}

#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    // Any failure lands on "no backend", which makes the manager refuse to run
    // commands at all. That is the correct direction: every alternative
    // publishes a confinement claim this host has not been shown to hold.
    match enforced_capabilities() {
        Ok(capabilities) => capabilities,
        Err(detail) => SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            filesystem: Enforcement::None,
            network: Enforcement::None,
            detail,
        },
    }
}

fn enforced_capabilities() -> Result<SandboxCapabilities, String> {
    probe().map_err(|error| format!("Landlock unavailable: {error}"))?;
    // The network claim below is a claim about a seccomp filter, so it may only
    // be made where one can be built. `target_arch` is the only thing that knows,
    // and `capabilities()` never used to ask: on a Linux this crate compiles for
    // but seccompiler has no `TargetArch` for (riscv64, s390x, ppc64le, armv7),
    // the report said `Full`/`Full` and `doctor` said "enforcing" while
    // `build_seccomp` failed for every command — an overclaim paired with a tool
    // that could not run anything, and no diagnostic naming seccomp.
    target_arch().map_err(|error| format!("seccomp unavailable: {error}"))?;

    let filesystem = Enforcement::from_gaps(landlock_gaps()?);
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
    Ok(SandboxCapabilities {
        backend: SandboxBackend::LinuxLandlock,
        available: true,
        filesystem,
        // Full, and unlike Landlock not negotiated per kernel: the filter either
        // loads or the command is refused. That is a claim about the filter
        // *loading*, so it only holds while nothing can submit network work
        // behind seccomp's back — which is why io_uring is denied on every
        // policy, and why the escape syscalls that hand over a ready-made socket
        // (`pidfd_getfd`) or another process's memory (`process_vm_readv`) are in
        // `DANGEROUS_SYSCALLS`.
        network: Enforcement::Full,
        detail,
    })
}

/// Truthful availability probe: `HardRequirement` makes ruleset creation error
/// when the kernel lacks Landlock, instead of silently no-opping.
fn probe() -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write_access(PROBE_ABI))
        .and_then(|ruleset| ruleset.create())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Whether this kernel can *handle* every write-class right `abi` defines.
///
/// `HardRequirement` is what makes this an answer rather than a guess: it turns
/// an unsupported right into an error instead of a silent downgrade.
///
/// Deliberately stops at `handle_access` and never calls `create()`. Under
/// `HardRequirement` the crate resolves compatibility against the running
/// kernel's ABI, which it learns once from
/// `landlock_create_ruleset(NULL, 0, …_VERSION)` — a call that returns a version
/// number, not a descriptor — so the verdict is already final at this point.
/// Creating a ruleset would only add a file descriptor per probe and, far worse,
/// fold resource failures into the verdict: an EMFILE under fd pressure used to
/// be indistinguishable from "this kernel lacks the right", and
/// [`super::detect_capabilities`] memoizes for the process lifetime, so one
/// transient error permanently taught every surface to report a fabricated ABI
/// gap on a host that was in fact enforcing it.
fn handles_write_rights(abi: ABI) -> Result<bool, String> {
    match Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write_access(abi))
    {
        Ok(_) => Ok(true),
        // The one error a `HardRequirement` compatibility refusal produces, and
        // the only one that means "this kernel is missing the right".
        Err(RulesetError::HandleAccesses(_)) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
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
/// Only a compatibility refusal counts as a gap; anything else is propagated so
/// the caller can refuse rather than publish a diagnosis it did not earn (see
/// [`handles_write_rights`]).
fn landlock_gaps() -> Result<Vec<EnforcementGap>, String> {
    let mut gaps = Vec::new();
    for (abi, gap) in WRITE_RIGHT_LEVELS {
        if !handles_write_rights(*abi)? {
            gaps.push(*gap);
        }
    }
    Ok(gaps)
}

/// The model-facing sentence for the deliberate device-ioctl denial. Kept in
/// sync with [`build_landlock`], which strips `IOCTL_DEV` from the `/dev`
/// grants — if that grant ever comes back, this sentence becomes a lie.
const DEVICE_IOCTL_DESIGN_NOTE: &str = "This sandbox deliberately refuses \
    ioctl(2) on device nodes (an ioctl on the controlling terminal reaches \
    TIOCSTI — keystroke injection into the user's shell), so allocating a \
    pseudo-terminal fails inside it: expect/pexpect/script-style tools report \
    'Permission denied' from /dev/ptmx. That refusal is by design and names no \
    path — /add-dir cannot fix it; run such tools in non-interactive (pipe) \
    mode instead.";

/// What this backend refuses on purpose, told to the model only where the
/// refusal is actually live (see [`super::sandbox_design_notes`]).
///
/// The ioctl note is conditional on the kernel *governing* the right at all —
/// ABI 5+, i.e. the [`EnforcementGap::LandlockIoctlDev`] gap is absent. Below
/// that the right is never checked, ptys work, and the gap's own
/// `model_caveat` describes the exposure instead. Note and gap are therefore
/// mutually exclusive by construction; a test in `super` pins that, because a
/// description carrying both would tell the model device ioctl is
/// simultaneously unchecked and refused.
pub(super) fn design_notes() -> &'static [&'static str] {
    let capabilities = super::detect_capabilities();
    let ioctl_governed = capabilities.available
        && !capabilities
            .filesystem
            .gaps()
            .contains(&EnforcementGap::LandlockIoctlDev);
    if ioctl_governed {
        &[DEVICE_IOCTL_DESIGN_NOTE]
    } else {
        &[]
    }
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
            // `restrict_self` consumes the ruleset and `pre_exec` takes an
            // `FnMut`, so the ruleset lives in an `Option`. A second spawn of
            // the same `Command` therefore finds `None` — which must be an
            // error, not a skip: silently applying seccomp without Landlock
            // would exec a child with unrestricted writes while every surface
            // still reported it confined. Fail-closed, matching the module doc.
            let created = ruleset.take().ok_or_else(|| {
                io::Error::other(
                    "landlock: sandboxed Command was spawned twice; the ruleset is \
                     single-use, so the second child would run unconfined",
                )
            })?;
            created
                .restrict_self()
                .map_err(|error| io::Error::other(format!("landlock: {error}")))?;
            for program in &bpf {
                seccompiler::apply_filter(program)
                    .map_err(|error| io::Error::other(format!("seccomp: {error}")))?;
            }
            Ok(())
        });
    }
    Ok(cmd)
}

fn build_restrictions(
    granted_roots: &[PathBuf],
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<(RulesetCreated, Vec<BpfProgram>), String> {
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
    // Device nodes are granted write so redirections work (`2> /dev/null`,
    // `echo x > /dev/tty`) — nothing more. Handing them the full write set also
    // hands them `IOCTL_DEV`, which is a different power entirely: `ioctl` on a
    // terminal reaches `TIOCSTI`, which pushes characters into the controlling
    // terminal's input queue for the user's own shell to execute after this
    // process exits — outside Landlock, seccomp, the deny floor and the approval
    // gate alike. Redirection needs the write bit, not the ioctl bit, so grant
    // only what it needs. The visible cost — pty allocation failing on kernels
    // that enforce the right — is disclosed to the model through
    // [`DEVICE_IOCTL_DESIGN_NOTE`]; restoring this grant would orphan that text.
    let device_access = access & !AccessFs::IoctlDev;
    // Pre-filter to existing paths so a missing /dev node can't fail the whole
    // ruleset build (path_beneath_rules errors on paths it can't open).
    let existing = |paths: Vec<PathBuf>| -> Vec<PathBuf> {
        paths.into_iter().filter(|path| path.exists()).collect()
    };
    let writable = existing(writable_paths(granted_roots, cwd, policy));
    let devices = existing(device_paths());

    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access)
        .and_then(|ruleset| ruleset.create())
        .and_then(|created| created.add_rules(path_beneath_rules(&writable, access)))
        .and_then(|created| created.add_rules(path_beneath_rules(&devices, device_access)))
        .map_err(|error| format!("landlock ruleset: {error}"))
}

fn writable_paths(granted_roots: &[PathBuf], cwd: &Path, policy: &SandboxPolicy) -> Vec<PathBuf> {
    // `writable_roots` already includes the temp dir (shared with the macOS
    // profile so the two backends cannot diverge on it again).
    policy.writable_roots(granted_roots, cwd)
}

/// `/dev` nodes a shell needs for redirections. Granted `device_access` (write
/// without `IOCTL_DEV`) rather than the full write set — see [`build_landlock`].
fn device_paths() -> Vec<PathBuf> {
    [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
        "/dev/random",
        "/dev/urandom",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Syscalls this policy denies with EPERM. An empty rule vector means "no
/// argument condition": every call of that number takes the filter's match
/// action. `clone` is the one *conditional* entry — an unconditional deny
/// there would deny fork itself.
///
/// Split out from [`build_seccomp`] because the compiled BPF is opaque, and
/// "which syscalls does a given policy actually deny" is the property worth
/// testing.
fn seccomp_rules(
    policy: &SandboxPolicy,
) -> Result<BTreeMap<i64, Vec<seccompiler::SeccompRule>>, String> {
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for syscall in DANGEROUS_SYSCALLS {
        rules.insert(*syscall, Vec::new());
    }
    rules.insert(libc::SYS_clone, vec![deny_new_user_namespace()?]);
    if !policy.has_network_access() {
        for syscall in NETWORK_SYSCALLS {
            rules.insert(*syscall, Vec::new());
        }
    }
    Ok(rules)
}

/// The rule refusing `clone(2)` when `CLONE_NEWUSER` is among its flags.
///
/// [`DANGEROUS_SYSCALLS`] already refuses namespace *entry* (`unshare`,
/// `setns`); this closes *creation*, which stayed reachable by spelling the
/// same request as a process-creation flag. Landlock and seccomp both survive
/// a namespace transition, so this is not about the write or network boundary
/// — a fresh user namespace grants full capabilities inside itself, which is
/// the gateway most local privilege-escalation exploits of the last decade
/// walk through, and nothing a build or test has a legitimate claim to.
///
/// The flags are argument 0 in the raw calling convention of both
/// architectures [`target_arch`] admits, and `MaskedEq` keys on the single
/// `CLONE_NEWUSER` bit: threads (`CLONE_THREAD|CLONE_VM|…`) and plain forks
/// carry any other combination and pass untouched. Known collateral: browser
/// sandboxes launched *inside* this sandbox (Puppeteer/Playwright headless
/// Chromium) clone with `CLONE_NEWUSER` and must run `--no-sandbox`, the same
/// answer they already need on distros that restrict unprivileged userns.
fn deny_new_user_namespace() -> Result<seccompiler::SeccompRule, String> {
    let new_user = libc::CLONE_NEWUSER as u64;
    let condition = seccompiler::SeccompCondition::new(
        0,
        seccompiler::SeccompCmpArgLen::Qword,
        seccompiler::SeccompCmpOp::MaskedEq(new_user),
        new_user,
    )
    .map_err(|error| format!("seccomp clone condition: {error}"))?;
    seccompiler::SeccompRule::new(vec![condition])
        .map_err(|error| format!("seccomp clone rule: {error}"))
}

/// The ENOSYS-denied set, kept apart from [`seccomp_rules`] because it carries
/// a different errno: io_uring (see [`IO_URING_SYSCALLS`]) plus `clone3`.
///
/// `clone3` cannot be argument-filtered — its flags live in a userspace struct
/// and seccomp reads registers, not memory — so allowing it would leave
/// `CLONE_NEWUSER` reachable by the modern spelling while the classic one is
/// refused. It cannot take EPERM either: glibc only falls back to `clone(2)`
/// on ENOSYS and treats EPERM as a real answer (the Docker seccomp incident —
/// `fork` failing with "Operation not permitted" across every container — is
/// this exact mistake, and EPERM would additionally trip
/// `write_denial_signature`, same as io_uring did). ENOSYS is what a pre-5.3
/// kernel returns, so every libc and runtime already answers it by using
/// `clone(2)` — where [`deny_new_user_namespace`] is waiting.
fn enosys_rules() -> BTreeMap<i64, Vec<seccompiler::SeccompRule>> {
    IO_URING_SYSCALLS
        .iter()
        .copied()
        .chain([libc::SYS_clone3])
        .map(|syscall| (syscall, Vec::new()))
        .collect()
}

/// Two filters, because a `SeccompFilter` carries ONE match action and the
/// ENOSYS set must not answer with the same errno as everything else.
///
/// EPERM on the ring was a real bug, not a cosmetic one: `write_denial_signature`
/// classifies "Operation not permitted" from a sandboxed command as a *filesystem
/// boundary denial*, so a `cargo test` on a `tokio-uring` crate got
/// [`super::WRITE_DENIAL_NOTE`] appended — telling the model to ask for
/// `/add-dir` over a failure that has nothing to do with paths — and three in one
/// turn tripped the runtime's boundary-denial circuit breaker, which is also
/// exempt from cascade escalation. So the real failure could never escalate.
///
/// ENOSYS is both inert to that classifier and the *truthful* answer: it is what
/// a kernel without io_uring (or without `clone3`) returns, which is exactly the
/// case every user of those interfaces already handles by falling back —
/// epoll/blocking I/O for the ring, `clone(2)` for `clone3`.
///
/// Stacked filters are safe here: the kernel runs both and takes the
/// highest-precedence result. The ENOSYS set appears only in the ENOSYS filter
/// (the EPERM one lets it through to its `Allow` default), and
/// `SECCOMP_RET_ERRNO` outranks `SECCOMP_RET_ALLOW`, so ENOSYS is what the
/// caller sees.
fn build_seccomp(policy: &SandboxPolicy) -> Result<Vec<BpfProgram>, String> {
    let arch = target_arch()?;
    let compile = |rules, errno: i32, label: &str| -> Result<BpfProgram, String> {
        SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(errno as u32),
            arch,
        )
        .map_err(|error| format!("seccomp filter ({label}): {error}"))?
        .try_into()
        .map_err(|error| format!("seccomp compile ({label}): {error}"))
    };

    Ok(vec![
        compile(seccomp_rules(policy)?, libc::EPERM, "denied")?,
        compile(enosys_rules(), libc::ENOSYS, "enosys")?,
    ])
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
    fn io_uring_is_denied_under_every_policy_and_never_with_eperm() {
        // The ring is a second way to submit the work a syscall would do, and
        // seccomp only sees syscalls — an `IORING_OP_SOCKET`/`IORING_OP_CONNECT`
        // pair reaches the network without `socket(2)` or `connect(2)`. So this
        // is what lets `capabilities()` report the network dimension as `Full`
        // rather than `Partial`: if the ring were reachable, that report would
        // be the same kind of overclaim the filesystem gaps exist to avoid.
        let enosys = enosys_rules();
        for syscall in IO_URING_SYSCALLS {
            assert!(
                enosys.contains_key(syscall),
                "syscall {syscall} must be denied"
            );
        }

        // ...and it must stay out of the EPERM filter, on EVERY policy. EPERM is
        // what `write_denial_signature` reads as a filesystem boundary denial, so
        // an io_uring failure carrying it would get `WRITE_DENIAL_NOTE` appended
        // — sending the model after an `/add-dir` grant for a failure that has
        // no path in it — and three per turn would trip the boundary-denial
        // circuit breaker, which is exempt from cascade escalation.
        for policy in [
            SandboxPolicy::workspace_write(),
            SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
        ] {
            let denied = seccomp_rules(&policy).expect("EPERM rules must build");
            for syscall in IO_URING_SYSCALLS {
                assert!(
                    !denied.contains_key(syscall),
                    "io_uring syscall {syscall} must not be in the EPERM filter under {policy:?}"
                );
            }
        }
    }

    #[test]
    fn user_namespace_creation_is_refused_under_every_policy() {
        // Three spellings, three closures: `unshare`/`setns` sit in the
        // unconditional EPERM list, `clone` carries an argument rule keyed on
        // the CLONE_NEWUSER bit, and `clone3` gets ENOSYS so every libc walks
        // back to the filterable spelling. Checked under both policies because
        // a network grant must widen exactly one thing (sockets), not this.
        for policy in [
            SandboxPolicy::workspace_write(),
            SandboxPolicy::WorkspaceWrite {
                network_access: true,
            },
        ] {
            let rules = seccomp_rules(&policy).expect("EPERM rules must build");
            for syscall in [libc::SYS_unshare, libc::SYS_setns] {
                assert!(
                    rules
                        .get(&syscall)
                        .is_some_and(|conditions| conditions.is_empty()),
                    "namespace entry syscall {syscall} must be denied unconditionally"
                );
            }
            assert!(
                rules
                    .get(&libc::SYS_clone)
                    .is_some_and(|conditions| !conditions.is_empty()),
                "clone must be denied CONDITIONALLY under {policy:?}: no rule leaves \
                 CLONE_NEWUSER reachable, an unconditional one denies fork itself"
            );
            assert!(
                !rules.contains_key(&libc::SYS_clone3),
                "clone3 belongs to the ENOSYS filter alone: glibc only falls back to \
                 clone(2) on ENOSYS, and an EPERM entry here would race that answer"
            );
        }
        assert!(
            enosys_rules().contains_key(&libc::SYS_clone3),
            "clone3 must be answered ENOSYS so libcs fall back to clone(2)"
        );
    }

    #[test]
    fn every_write_right_the_ruleset_requests_has_a_gap_to_report() {
        // `build_landlock` asks for `write_access(LANDLOCK_ABI)` under
        // `BestEffort`, which drops unsupported rights SILENTLY — `landlock_gaps`
        // is the only thing that turns that silence into a report, and it probes
        // exactly the levels named in `WRITE_RIGHT_LEVELS`. Nothing else ties the
        // two together, so raising `LANDLOCK_ABI` for a newly added right without
        // adding its gap here would make a kernel that cannot enforce it report
        // `Full` — the exact ABI-V1 truncate hole this module's docs describe as
        // having already shipped once, reintroduced one level up.
        let highest = WRITE_RIGHT_LEVELS
            .last()
            .expect("at least one write-right level")
            .0;
        assert_eq!(
            write_access(highest),
            write_access(LANDLOCK_ABI),
            "WRITE_RIGHT_LEVELS tops out below the ABI the ruleset actually requests: \
             a right gained by raising LANDLOCK_ABI would never be probed for"
        );

        // Each listed level must strictly widen the one below it. A level that
        // adds nothing is probing for a gap that cannot exist, which would
        // attach a gap label to a kernel that has the right — the understating
        // direction, but still a wrong answer on the majority of hosts.
        //
        // Note this walks the *listed* levels, not every ABI: ABI 2 is absent on
        // purpose (`REFER`), and it needs no entry of its own because a kernel
        // without it also fails the ABI 3 probe — `write_access(V3)` is a
        // superset, so the gap is reported either way.
        let mut previous = write_access(PROBE_ABI);
        for (abi, gap) in WRITE_RIGHT_LEVELS {
            let rights = write_access(*abi);
            assert!(
                rights.contains(previous),
                "{abi:?} must widen the level below it, not replace it"
            );
            assert_ne!(
                rights, previous,
                "{abi:?} ({gap:?}) adds no write-class right over the level below"
            );
            previous = rights;
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
