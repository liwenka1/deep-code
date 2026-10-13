use std::path::{Path, PathBuf};

use crate::execution_policy::ToolExecutionPlan;

/// Sandbox restrictions applied to shell commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// No OS sandbox wrapper.
    Unsandboxed,
    /// Read broadly; write only under workspace (and cwd).
    WorkspaceWrite { network_access: bool },
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            network_access: false,
        }
    }
}

impl SandboxPolicy {
    #[must_use]
    pub fn workspace_write() -> Self {
        Self::WorkspaceWrite {
            network_access: false,
        }
    }

    pub fn should_sandbox(&self) -> bool {
        !matches!(self, Self::Unsandboxed)
    }

    pub fn has_network_access(&self) -> bool {
        matches!(
            self,
            Self::WorkspaceWrite {
                network_access: true,
            }
        )
    }

    #[must_use]
    pub fn from_execution_plan(plan: Option<&ToolExecutionPlan>) -> Self {
        match plan {
            // Network rides the plan, not the trust decision: commands run
            // without egress unless the call declared `network: true` AND the
            // gate accounted for it (a forced approval under the default
            // `prompt` mode, or blanket `[sandbox] network = "always"`). Reads
            // stay broad in the profile, so an ambient network grant would
            // turn every auto-allowed command into an exfiltration path — a
            // no-network run can read `~/.ssh` but cannot send it anywhere.
            // See `NetworkMode` and macos_seatbelt.rs.
            Some(plan) if plan.requires_sandbox => Self::WorkspaceWrite {
                network_access: plan.network,
            },
            _ => Self::Unsandboxed,
        }
    }

    /// Writable roots: every granted root (the workspace first, then any
    /// `--add-dir` grants), the command's cwd when it is not already granted,
    /// the system temp dir, and (unix) `/tmp`.
    ///
    /// The temp dir is not a convenience. Toolchains write scratch files there
    /// unconditionally — `rustc` allocates a temp dir per invocation, git's
    /// xcrun shim caches there, every `mktemp` caller needs it — so a profile
    /// without it does not *confine* `cargo build`, it makes it fail outright.
    /// Granting it is no escalation either: the sandbox exists to hold commands
    /// inside the granted boundary, and `$TMPDIR` is per-user ephemeral space
    /// the user's own processes already write to. Both backends read this list,
    /// so the grant cannot drift per-platform again.
    ///
    /// `/tmp` rides the same argument. On Linux it IS the temp dir; on macOS
    /// the temp dir is `$TMPDIR` (`/var/folders/…`) and `/tmp` is a separate
    /// place tools land on when they cannot ask for the real one: inside the
    /// sandbox `confstr(_CS_DARWIN_USER_TEMP_DIR)` fails (the dirhelper mach
    /// service is behind deny-default), so Apple's xcrun shim — run by every
    /// `git` invocation — falls back to `/tmp` and, without this grant, spams
    /// "Operation not permitted" on stderr. That noise isn't cosmetic: it
    /// carries the write-denial signature, so a git run that failed for a
    /// different reason gets misclassified as a write-boundary denial.
    /// Sticky world-writable `/tmp` is space every unconfined process already
    /// writes to; granting it widens nothing that matters.
    pub fn writable_roots(&self, granted: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
        match self {
            Self::Unsandboxed => Vec::new(),
            Self::WorkspaceWrite { .. } => {
                let mut roots: Vec<PathBuf> = Vec::new();
                for root in granted {
                    if !roots.contains(root) {
                        roots.push(root.clone());
                    }
                }
                if !roots.iter().any(|existing| existing == cwd) {
                    roots.push(cwd.to_path_buf());
                }
                let temp = std::env::temp_dir();
                if !roots.contains(&temp) {
                    roots.push(temp);
                }
                if cfg!(unix) {
                    let tmp = PathBuf::from("/tmp");
                    if !roots.contains(&tmp) {
                        roots.push(tmp);
                    }
                }
                // Linux POSIX shared memory is files under `/dev/shm` (a tmpfs):
                // Python multiprocessing (Pool/Queue/Lock/ProcessPoolExecutor),
                // PyTorch DataLoader workers and headless Chromium all create
                // objects there, and without the grant Landlock refuses the write
                // with EACCES and those everyday workloads fail. macOS covers the
                // equivalent through `ipc-posix-sem`. Linux-only: `/dev/shm` does
                // not exist on macOS, where it would be a dead (or rejected) root.
                if cfg!(target_os = "linux") {
                    let shm = PathBuf::from("/dev/shm");
                    if !roots.contains(&shm) {
                        roots.push(shm);
                    }
                }
                roots
            }
        }
    }

    /// [`Self::writable_roots`] as one spawn grants them: each resolved to the
    /// real path the kernel matches — except that a root handed over already
    /// resolved is left out when it no longer resolves to itself.
    ///
    /// Two kinds of root are in that list, and they resolve differently. The
    /// temp dirs are system paths behind system symlinks (`/tmp` is
    /// `/private/tmp` on macOS), resolved here per spawn. Every other root — the
    /// granted ones and the cwd — was resolved ONCE, when it was granted
    /// (`WorkspacePolicy`, `resolve_cwd`, `paths::tool_cache_roots`), and is
    /// pinned to that path.
    ///
    /// Re-resolving those per spawn was a write-fence escape. A sandboxed
    /// command may write inside a granted root, so it can move the directory a
    /// nested root sits in — workspace `/tmp/a/ws` with `/tmp` granted, or
    /// `~/code/org/repo` beside `--add-dir ~/code` — and put a symlink where
    /// the root was: `mv /tmp/a /tmp/a2 && mkdir /tmp/a && ln -s ~ /tmp/a/ws`.
    /// The next spawn resolved the workspace to `$HOME` and granted that.
    /// Locking each root's own entry against rename did not reach the
    /// directories between two roots, and it stopped legitimate removals of a
    /// cache root or a cwd root besides. A pinned root that resolves somewhere
    /// else now is a directory that was moved or replaced since it was granted,
    /// and it is not granted again under its old name.
    ///
    /// A root that does not resolve at all (it does not exist) passes through
    /// as written, as it always did: nothing stands behind it to escape to, and
    /// a symlink later planted in its place fails this test on the spawn after.
    pub fn resolved_writable_roots(&self, granted: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
        let mut resolved: Vec<PathBuf> = Vec::new();
        for root in self.writable_roots(granted, cwd) {
            let pinned = root == cwd || granted.contains(&root);
            let real = match crate::paths::canonicalize(&root) {
                Ok(real) if pinned && real != root => continue,
                Ok(real) => real,
                Err(_) => root,
            };
            if !resolved.contains(&real) {
                resolved.push(real);
            }
        }
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::{PolicyVerdict, RiskLevel, ToolExecutionPlan};

    fn plan(requires_sandbox: bool, network: bool) -> ToolExecutionPlan {
        ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: None,
            network,
        }
    }

    /// A granted root (or the cwd) is pinned to the path it was granted at: one
    /// a symlink now stands at is left out — under its old name and at the
    /// symlink's target alike — while the system temp roots are still resolved
    /// per spawn, and a root that does not exist passes through as written.
    /// Both backends read this one list, so this is the pin for each of them.
    #[cfg(unix)]
    #[test]
    fn a_pinned_root_that_resolves_elsewhere_now_is_left_out() {
        let holder = tempfile::tempdir().unwrap();
        let holder = holder.path().canonicalize().unwrap();
        let real = holder.join("real");
        std::fs::create_dir(&real).unwrap();
        let target = holder.join("target");
        std::fs::create_dir(&target).unwrap();
        let swapped = holder.join("swapped");
        std::os::unix::fs::symlink(&target, &swapped).unwrap();
        let missing = holder.join("missing");

        let policy = SandboxPolicy::workspace_write();
        let roots = policy
            .resolved_writable_roots(&[real.clone(), swapped.clone(), missing.clone()], &real);
        assert!(roots.contains(&real), "{roots:?}");
        assert!(roots.contains(&missing), "{roots:?}");
        assert!(
            !roots.contains(&swapped) && !roots.contains(&target),
            "{roots:?}"
        );
        // A swapped cwd is pinned the same way.
        let roots = policy.resolved_writable_roots(std::slice::from_ref(&real), &swapped);
        assert!(!roots.contains(&target), "{roots:?}");
        // The temp roots are resolved, not pinned: `/tmp` may be a system
        // symlink (macOS) and is granted at its real path.
        let tmp = crate::paths::canonicalize(Path::new("/tmp")).unwrap();
        assert!(roots.contains(&tmp), "{roots:?}");
    }

    /// `/tmp` must be writable on unix even when the process temp dir is
    /// elsewhere (macOS: `$TMPDIR` under /var/folders). Tools that cannot ask
    /// for the real temp dir fall back to `/tmp` — inside the sandbox
    /// `confstr(_CS_DARWIN_USER_TEMP_DIR)` fails, so Apple's xcrun shim (run
    /// by every git invocation) lands exactly there, and without the grant
    /// its EPERM noise misclassifies unrelated failures as write denials.
    #[cfg(unix)]
    #[test]
    fn tmp_is_always_a_writable_root_on_unix() {
        let ws = Path::new("/w");
        let roots = SandboxPolicy::workspace_write().writable_roots(&[ws.to_path_buf()], ws);
        assert!(
            roots.contains(&PathBuf::from("/tmp")),
            "unix roots must include /tmp: {roots:?}"
        );
        // And exactly once, even when the temp dir already is /tmp (Linux).
        assert_eq!(
            roots
                .iter()
                .filter(|root| *root == &PathBuf::from("/tmp"))
                .count(),
            1
        );
    }

    /// Linux POSIX shared memory needs `/dev/shm` writable, or multiprocessing
    /// and DataLoader workloads fail with EACCES under Landlock.
    #[cfg(target_os = "linux")]
    #[test]
    fn dev_shm_is_a_writable_root_on_linux() {
        let ws = Path::new("/w");
        let roots = SandboxPolicy::workspace_write().writable_roots(&[ws.to_path_buf()], ws);
        assert!(
            roots.contains(&PathBuf::from("/dev/shm")),
            "linux roots must include /dev/shm: {roots:?}"
        );
    }

    #[test]
    fn execution_plan_maps_to_sandbox_policy() {
        assert!(matches!(
            SandboxPolicy::from_execution_plan(None),
            SandboxPolicy::Unsandboxed
        ));
        assert!(matches!(
            SandboxPolicy::from_execution_plan(Some(&plan(false, false))),
            SandboxPolicy::Unsandboxed
        ));
        // The decoupling: a sandboxed command WITHOUT a vetted network grant
        // runs with egress (and listening) blocked. Filesystem confinement
        // and the credential-dir write denials are unaffected.
        assert_eq!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, false))),
            SandboxPolicy::WorkspaceWrite {
                network_access: false
            }
        );
        // Only a plan the gate marked (declared + approved, or config
        // `always`) carries network into the sandbox.
        assert_eq!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, true))),
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
        );
        assert!(!SandboxPolicy::from_execution_plan(Some(&plan(true, false))).has_network_access());
    }

    /// The policy bit every sandbox decision starts from, pinned in BOTH
    /// directions. Mutation testing showed either collapse survived the whole
    /// suite: `-> false` runs every confined command bare (the kernel-level
    /// seatbelt tests pin the wrapper, not this decision), `-> true` wraps
    /// trusted bare runs. Neither may regress silently again.
    #[test]
    fn should_sandbox_is_pinned_in_both_directions() {
        assert!(SandboxPolicy::workspace_write().should_sandbox());
        assert!(
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
            .should_sandbox()
        );
        assert!(!SandboxPolicy::Unsandboxed.should_sandbox());
    }
}
