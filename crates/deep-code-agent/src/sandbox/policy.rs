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
                for root in granted
                    .iter()
                    .cloned()
                    .chain(std::iter::once(cwd.to_path_buf()))
                    .chain(system_roots())
                {
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                roots
            }
        }
    }

    /// [`Self::writable_roots`] as one spawn grants them: each resolved to the
    /// real path the kernel matches, and a root left out when what now stands
    /// at its path is not what may be granted there.
    ///
    /// Re-resolving a root per spawn is a write-fence escape whenever a
    /// sandboxed command can put a symlink where the root was. It can, more
    /// often than it looks: it may write inside every granted root, so it can
    /// move the directory a nested root sits in — workspace `/tmp/a/ws` with
    /// `/tmp` granted, or `~/code/org/repo` beside `--add-dir ~/code` — and on
    /// macOS a `subpath` grant covers the root's OWN entry, so it can move a
    /// root itself, `$TMPDIR` included (`mv "$TMPDIR" /tmp/x && ln -s ~
    /// "$TMPDIR"`). The next spawn resolved the root to `$HOME` and granted
    /// that. Locking each root's entry against rename reached neither the
    /// directories between two roots nor Linux, and refused legitimate removals
    /// of a cache root or a cwd root besides. So each kind of root is checked
    /// for what was put there instead:
    ///
    /// - **Granted roots and the cwd** were resolved ONCE, when they were
    ///   granted (`WorkspacePolicy`, `resolve_cwd`, `paths::tool_cache_roots`),
    ///   and are pinned to that path: one that resolves anywhere else now is a
    ///   directory moved or replaced since, and is not granted under its old
    ///   name.
    /// - **The system temp roots** ([`system_roots`]) are spelled through
    ///   system symlinks (`/tmp` is `/private/tmp`, `$TMPDIR` lives under `/var`
    ///   on macOS), so they are resolved here — but only through symlinks this
    ///   user could not have planted ([`resolve_system_root`]). That also holds
    ///   across sessions, where a pin taken at startup would have pinned a
    ///   symlink a previous session planted.
    ///
    /// A root that does not resolve at all (it does not exist) passes through
    /// as written, as it always did: nothing stands behind it to escape to, and
    /// a symlink later planted in its place fails these tests on the spawn after.
    ///
    /// Race-free on both backends, because what is granted is the RESOLVED
    /// path: Seatbelt matches it against the real path of each operation, and
    /// Landlock opens it without following any symlink (`linux.rs`), so a swap
    /// after this check moves nothing.
    pub fn resolved_writable_roots(&self, granted: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
        self.resolved_writable_roots_with(granted, cwd, &system_roots())
    }

    /// [`Self::resolved_writable_roots`] with the system roots supplied, so the
    /// check on them is assertable without changing the process's `$TMPDIR`.
    fn resolved_writable_roots_with(
        &self,
        granted: &[PathBuf],
        cwd: &Path,
        system: &[PathBuf],
    ) -> Vec<PathBuf> {
        if !self.should_sandbox() {
            return Vec::new();
        }
        let pinned = granted
            .iter()
            .cloned()
            .chain(std::iter::once(cwd.to_path_buf()))
            .map(|root| resolve_pinned_root(&root));
        let system = system.iter().map(|root| resolve_system_root(root));
        let mut resolved: Vec<PathBuf> = Vec::new();
        for real in pinned.chain(system).flatten() {
            if !resolved.contains(&real) {
                resolved.push(real);
            }
        }
        resolved
    }
}

/// The roots every sandboxed command may write besides the granted ones: the
/// temp dir, `/tmp` (unix) and `/dev/shm` (Linux). See
/// [`SandboxPolicy::writable_roots`] for why each is there.
fn system_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    if cfg!(unix) {
        roots.push(PathBuf::from("/tmp"));
    }
    // Linux POSIX shared memory is files under `/dev/shm` (a tmpfs): Python
    // multiprocessing (Pool/Queue/Lock/ProcessPoolExecutor), PyTorch DataLoader
    // workers and headless Chromium all create objects there, and without the
    // grant Landlock refuses the write with EACCES and those everyday workloads
    // fail. macOS covers the equivalent through `ipc-posix-sem`. Linux-only:
    // `/dev/shm` does not exist on macOS, where it would be a dead (or
    // rejected) root.
    if cfg!(target_os = "linux") {
        roots.push(PathBuf::from("/dev/shm"));
    }
    roots
}

/// A granted root or cwd, resolved — or `None` when it no longer resolves to
/// the path it was granted at (see [`SandboxPolicy::resolved_writable_roots`]).
fn resolve_pinned_root(root: &Path) -> Option<PathBuf> {
    match crate::paths::canonicalize(root) {
        Ok(real) if real != root => None,
        Ok(real) => Some(real),
        Err(_) => Some(root.to_path_buf()),
    }
}

/// A system temp root, resolved by following only the symlinks root owns — or
/// `None` when its path runs through one this user could have planted.
///
/// A system root is MEANT to be reached through symlinks (`/tmp`, `/var` on
/// macOS), so "resolves to itself" is no test for it. What a sandboxed command
/// can put in its path is a symlink it owns; the system's own are owned by
/// root. The cost is a `$TMPDIR` the user made a symlink themselves, which is
/// indistinguishable from a planted one and is no longer granted.
///
/// Resolved here, one component at a time, rather than checked and then
/// handed to `canonicalize`: a sandboxed background job swapping a link in and
/// out could land it between a check and a second resolution. The path
/// returned is built from the very observations that were checked, so it names
/// what was a real directory when looked at; a symlink swapped in afterwards is
/// matched by nothing (Seatbelt compares real paths, Landlock opens it without
/// following). A tail that does not exist is kept as written, as before.
#[cfg(unix)]
fn resolve_system_root(root: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    /// More hops than any system spelling takes, fewer than a loop needs.
    const MAX_LINKS: usize = 40;
    // A stack, so a path's first component is pushed last.
    let push_components = |rest: &mut Vec<OsString>, path: &Path| {
        for component in path.components().rev() {
            match component {
                Component::Normal(name) => rest.push(name.to_os_string()),
                Component::ParentDir => rest.push(OsString::from("..")),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
    };
    if !root.is_absolute() {
        return None;
    }
    let mut rest: Vec<OsString> = Vec::new();
    push_components(&mut rest, root);
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = rest.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
            resolved = candidate;
            while let Some(name) = rest.pop() {
                resolved.push(name);
            }
            return Some(resolved);
        };
        if !meta.file_type().is_symlink() {
            resolved = candidate;
            continue;
        }
        links += 1;
        if meta.uid() != 0 || links > MAX_LINKS {
            return None;
        }
        let target = std::fs::read_link(&candidate).ok()?;
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        push_components(&mut rest, &target);
    }
    Some(resolved)
}

/// No backend confines writes by path here; resolved the ordinary way.
#[cfg(not(unix))]
fn resolve_system_root(root: &Path) -> Option<PathBuf> {
    Some(crate::paths::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
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

    /// A system temp root is reached through symlinks on purpose (`/tmp`,
    /// `/var` on macOS), so it is not pinned — but one reached through a
    /// symlink this user owns is left out, at the symlink's target and under its
    /// own name: that is what `mv "$TMPDIR" /tmp/x && ln -s ~ "$TMPDIR"` leaves
    /// behind, last component or any other. The root-owned system links still
    /// resolve.
    #[cfg(unix)]
    #[test]
    fn a_system_root_reached_through_a_planted_symlink_is_left_out() {
        let holder = tempfile::tempdir().unwrap();
        let holder = holder.path().canonicalize().unwrap();
        let ws = holder.join("ws");
        let target = holder.join("target");
        let real = holder.join("real-temp");
        for dir in [&ws, &target, &real, &target.join("T")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let planted = holder.join("T");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        let via = holder.join("via");
        std::os::unix::fs::symlink(&target, &via).unwrap();

        let roots = SandboxPolicy::workspace_write().resolved_writable_roots_with(
            std::slice::from_ref(&ws),
            &ws,
            &[
                planted.clone(),
                via.join("T"),
                real.clone(),
                PathBuf::from("/tmp"),
            ],
        );
        assert!(
            !roots.contains(&target) && !roots.contains(&target.join("T")),
            "{roots:?}"
        );
        assert!(roots.contains(&real), "{roots:?}");
        let tmp = crate::paths::canonicalize(Path::new("/tmp")).unwrap();
        assert!(roots.contains(&tmp), "{roots:?}");
        // Resolved by hand, a system root lands exactly where `canonicalize`
        // does when nothing was planted.
        let temp = std::env::temp_dir();
        assert_eq!(
            resolve_system_root(&temp),
            Some(crate::paths::canonicalize(&temp).unwrap())
        );
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
