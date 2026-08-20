use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::tool::ToolError;

/// The directories a session may write to: one primary workspace plus zero or
/// more extra roots granted explicitly at launch (`--add-dir`).
///
/// The primary root keeps every anchor role it always had — relative paths
/// resolve against it, sessions/checkpoints/config live under it, displays
/// strip it. Extra roots add *containment zones* only: an absolute path may
/// resolve into one, and the OS sandbox turns each into a write grant, but
/// nothing else about them is special. Keeping the primary distinguished is
/// what lets every single-root call site stay a one-liner (`From<PathBuf>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoots {
    pub primary: PathBuf,
    pub extras: Vec<PathBuf>,
}

impl WorkspaceRoots {
    #[must_use]
    pub fn new(primary: impl Into<PathBuf>, extras: Vec<PathBuf>) -> Self {
        Self {
            primary: primary.into(),
            extras,
        }
    }
}

impl From<PathBuf> for WorkspaceRoots {
    fn from(primary: PathBuf) -> Self {
        Self {
            primary,
            extras: Vec::new(),
        }
    }
}

impl From<&Path> for WorkspaceRoots {
    fn from(primary: &Path) -> Self {
        Self {
            primary: primary.to_path_buf(),
            extras: Vec::new(),
        }
    }
}

/// One session-wide boundary, shared live by every holder of a clone.
///
/// The primary root is a plain immutable field — its anchor roles (relative
/// paths, spill/checkpoint/session homes, display stripping) never move for
/// the life of a session. Only the extra grants sit behind the shared lock,
/// and the sole mutation is [`Self::grant_resolved`]: an append that runs
/// after an explicit human approval (`/add-dir` relaunch aside, which
/// rebuilds the policy wholesale). Cloning shares the boundary on purpose —
/// that is what lets a mid-session grant reach every already-registered tool
/// (and the sandbox, which re-reads [`Self::granted_roots`] per command)
/// without rebuilding a single registry.
#[derive(Debug, Clone)]
pub(crate) struct WorkspacePolicy {
    /// Canonical primary workspace; never changes after construction.
    primary: PathBuf,
    /// Canonical extra grants (deduped, never equal to the primary). Shared:
    /// every clone of this policy sees an approved grant immediately.
    extras: Arc<RwLock<Vec<PathBuf>>>,
}

/// What [`WorkspacePolicy::grant_extra`] did with an approved directory.
pub(crate) enum RootGrantOutcome {
    /// Appended as a new writable root.
    Granted { canonical: PathBuf },
    /// Already inside the boundary (an existing root or covered by one) —
    /// nothing changed, writes there work today.
    AlreadyGranted { canonical: PathBuf },
}

impl WorkspacePolicy {
    pub(crate) fn new(roots: impl Into<WorkspaceRoots>) -> Result<Self, ToolError> {
        let WorkspaceRoots { primary, extras } = roots.into();
        let canonical_primary = primary.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                "workspace",
                format!(
                    "failed to resolve workspace root {}: {error}",
                    primary.display()
                ),
            )
        })?;
        let mut canonical_extras: Vec<PathBuf> = Vec::new();
        for extra in extras {
            // A grant that cannot be resolved refuses the launch instead of
            // silently narrowing the session: the user explicitly asked for
            // this directory to be writable, and proceeding with fewer rights
            // than they believe they granted is how confusing mid-task
            // denials happen.
            let canonical = extra.canonicalize().map_err(|error| {
                ToolError::exec_failed(
                    "workspace",
                    format!(
                        "failed to resolve --add-dir root {}: {error}",
                        extra.display()
                    ),
                )
            })?;
            if !canonical.is_dir() {
                return Err(ToolError::exec_failed(
                    "workspace",
                    format!("--add-dir root {} is not a directory", extra.display()),
                ));
            }
            if canonical != canonical_primary && !canonical_extras.contains(&canonical) {
                canonical_extras.push(canonical);
            }
        }
        Ok(Self {
            primary: canonical_primary,
            extras: Arc::new(RwLock::new(canonical_extras)),
        })
    }

    /// The primary workspace root (canonical).
    pub(crate) fn root(&self) -> &Path {
        &self.primary
    }

    /// Every granted root (canonical), primary first. This exact list is what
    /// the OS sandbox turns into write grants, so tool-layer containment and
    /// kernel confinement cannot drift apart. Snapshot semantics: taken fresh
    /// per call, so a command spawned after a mid-session grant is confined to
    /// the widened boundary while one already running keeps the old one.
    pub(crate) fn granted_roots(&self) -> Vec<PathBuf> {
        let extras = self
            .extras
            .read()
            .expect("workspace boundary lock poisoned");
        let mut roots = Vec::with_capacity(1 + extras.len());
        roots.push(self.primary.clone());
        roots.extend(extras.iter().cloned());
        roots
    }

    /// A point-in-time [`WorkspaceRoots`] view (for summaries, prompts, and
    /// approval previews that want the value shape).
    pub(crate) fn to_roots(&self) -> WorkspaceRoots {
        WorkspaceRoots {
            primary: self.primary.clone(),
            extras: self
                .extras
                .read()
                .expect("workspace boundary lock poisoned")
                .clone(),
        }
    }

    /// Resolve and vet a model-requested grant target, granting nothing.
    ///
    /// Validation mirrors launch-time extras (canonicalize, must be a
    /// directory) plus three request-channel rules. The path must be spelled
    /// absolute — the requester is the model, and a relative spelling is
    /// ambiguous about which base it meant. The resolved name must be free of
    /// control characters, or the approval panel could not display it
    /// faithfully. And the resolved directory must not cover the home
    /// directory or be the filesystem root: the tool description already
    /// promises the model never to ask for those, and a promise the code
    /// does not check is one a symlink can break — a link inside the
    /// workspace can dress `$HOME` up as an innocuous-looking spelling. (A
    /// target already inside the boundary skips that last floor: covering it
    /// again changes nothing and reports as AlreadyGranted.)
    ///
    /// This is the single resolution step of the grant flow: the approval
    /// prompt displays exactly this canonical path, and after the human says
    /// yes the runtime resolves AGAIN and refuses on mismatch (see
    /// `apply_root_grant`) — so what was approved and what gets enforced
    /// cannot differ, no matter what happened to the path in between.
    pub(crate) fn resolve_grant_target(&self, raw: &Path) -> Result<PathBuf, ToolError> {
        const TOOL: &str = "request_write_root";
        if !raw.is_absolute() {
            return Err(invalid(
                TOOL,
                format!(
                    "path must be absolute, got '{}': spell out the full directory path",
                    raw.display()
                ),
            ));
        }
        let canonical = raw.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                TOOL,
                format!(
                    "cannot resolve {}: {error}; the directory must already exist",
                    raw.display()
                ),
            )
        })?;
        if !canonical.is_dir() {
            return Err(invalid(
                TOOL,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        // A name embedding control characters cannot be displayed faithfully
        // on the approval panel (an embedded newline or escape byte could
        // fabricate panel lines inside a security prompt), and the panel IS
        // the approval — so the request is refused before anyone is asked.
        // Legitimate directories don't carry control bytes in their names.
        if canonical.to_string_lossy().chars().any(char::is_control) {
            return Err(invalid(
                TOOL,
                format!(
                    "refusing {canonical:?}: the directory name contains control characters, \
                     which cannot be displayed faithfully in an approval prompt"
                ),
            ));
        }
        if !self.is_granted(&canonical) {
            if canonical.parent().is_none() {
                return Err(invalid(
                    TOOL,
                    "refusing to grant the filesystem root; request the narrowest directory \
                     that unblocks the task",
                ));
            }
            if let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok())
                && home.starts_with(&canonical)
            {
                return Err(invalid(
                    TOOL,
                    format!(
                        "refusing to grant '{}': it would make the entire home directory \
                         writable; request the narrowest directory that unblocks the task",
                        canonical.display()
                    ),
                ));
            }
            // Credential stores and deep-code's own config directory. The OS
            // sandbox already denies writes to exactly these under every
            // policy, precisely so that a writable root cannot reach them —
            // but `read_file`/`write_file` run in-process and never meet the
            // sandbox, so without this floor an approved grant would hand
            // over what the kernel fence refuses. `~/.deep-code` is the worst
            // of them: it holds the plaintext API key, and `auto_allow` is
            // honoured only from that file, so a write there outlives the
            // session the approval prompt scopes itself to.
            //
            // Overlap in EITHER direction is refused: the target may not be
            // inside a secret store, nor an ancestor that would cover one.
            // Deliberately narrower than `--add-dir`, which stays the human's
            // own call — here the model chooses the target and a plausible
            // spelling is all it takes.
            if let Some(secret) = sensitive_overlap(&canonical, &crate::paths::sensitive_paths()) {
                return Err(invalid(
                    TOOL,
                    format!(
                        "refusing to grant '{}': it overlaps the credential store '{}', \
                         which is never writable through a request; request the narrowest \
                         directory that unblocks the task",
                        canonical.display(),
                        secret.display()
                    ),
                ));
            }
        }
        Ok(canonical)
    }

    /// Widen the boundary with a target vetted by
    /// [`Self::resolve_grant_target`], after the human approved that exact
    /// path. Takes the canonical form on purpose: the caller proves it is
    /// granting what was displayed by passing the displayed value.
    pub(crate) fn grant_resolved(&self, canonical: PathBuf) -> RootGrantOutcome {
        let mut extras = self
            .extras
            .write()
            .expect("workspace boundary lock poisoned");
        // Covered by an existing root (including the primary): report rather
        // than record a redundant grant — writes there already work.
        if canonical.starts_with(&self.primary)
            || extras.iter().any(|root| canonical.starts_with(root))
        {
            return RootGrantOutcome::AlreadyGranted { canonical };
        }
        extras.push(canonical.clone());
        RootGrantOutcome::Granted { canonical }
    }

    /// One-step resolve-and-grant, for tests that need no prompt in between.
    /// The interactive flow deliberately never uses this: it resolves at
    /// prompt time, displays that path, and re-resolves at grant time,
    /// refusing on mismatch.
    #[cfg(test)]
    pub(crate) fn grant_extra(&self, raw: &Path) -> Result<RootGrantOutcome, ToolError> {
        Ok(self.grant_resolved(self.resolve_grant_target(raw)?))
    }

    fn is_granted(&self, canonical: &Path) -> bool {
        if canonical.starts_with(&self.primary) {
            return true;
        }
        self.extras
            .read()
            .expect("workspace boundary lock poisoned")
            .iter()
            .any(|root| canonical.starts_with(root))
    }

    pub(crate) fn resolve_cwd(
        &self,
        raw: Option<&str>,
        tool_name: &str,
    ) -> Result<PathBuf, ToolError> {
        let Some(raw) = raw else {
            return Ok(self.root().to_path_buf());
        };
        self.resolve_existing(raw, tool_name).and_then(|path| {
            if path.is_dir() {
                Ok(path)
            } else {
                Err(invalid(tool_name, "cwd must be a directory"))
            }
        })
    }

    pub(crate) fn resolve_existing(
        &self,
        raw: &str,
        tool_name: &str,
    ) -> Result<PathBuf, ToolError> {
        let candidate = self.prepare_candidate(raw, tool_name)?;
        if contains_symlink(&candidate, &self.granted_roots()).map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!("failed to inspect {}: {error}", candidate.display()),
            )
        })? {
            return Err(path_error(tool_name, raw, "symlinks are not allowed"));
        }
        let canonical = candidate.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!("failed to resolve {}: {error}", candidate.display()),
            )
        })?;
        if !self.is_granted(&canonical) {
            return Err(path_error(tool_name, raw, OUTSIDE_ROOTS));
        }
        Ok(canonical)
    }

    pub(crate) fn resolve_for_write(
        &self,
        raw: &str,
        tool_name: &str,
    ) -> Result<PathBuf, ToolError> {
        let candidate = self.prepare_candidate(raw, tool_name)?;
        if candidate.exists() {
            if contains_symlink(&candidate, &self.granted_roots()).map_err(|error| {
                ToolError::exec_failed(
                    tool_name,
                    format!("failed to inspect {}: {error}", candidate.display()),
                )
            })? {
                return Err(path_error(
                    tool_name,
                    raw,
                    "symlinks in the destination path are not allowed",
                ));
            }
            let canonical = candidate.canonicalize().map_err(|error| {
                ToolError::exec_failed(
                    tool_name,
                    format!("failed to resolve {}: {error}", candidate.display()),
                )
            })?;
            if !self.is_granted(&canonical) {
                return Err(path_error(tool_name, raw, OUTSIDE_ROOTS));
            }
            return Ok(candidate);
        }
        let parent = candidate.parent().ok_or_else(|| {
            path_error(
                tool_name,
                raw,
                "path must have a parent directory inside a granted root",
            )
        })?;
        // Missing parent directories are created rather than refused. A coding
        // agent adds new modules constantly, and failing `write_file` on
        // `src/new_mod/thing.rs` forced the model into a separate, separately
        // approved `mkdir -p` for a directory the write tool was already
        // authorized to create.
        //
        // Containment is decided on the deepest ancestor that actually exists —
        // canonicalized, and symlink-checked over the same existing portion —
        // *before* anything is created, so a symlinked or otherwise escaping
        // ancestor still cannot be used to write outside the granted roots. (A
        // non-existent path can be neither canonicalized nor stat'd, so the old
        // code failed here rather than at any deliberate check.)
        let mut existing = parent;
        while !existing.exists() {
            match existing.parent() {
                Some(next) => existing = next,
                None => break,
            }
        }
        if contains_symlink(existing, &self.granted_roots()).map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!("failed to inspect {}: {error}", existing.display()),
            )
        })? {
            return Err(path_error(
                tool_name,
                raw,
                "symlinks in the destination path are not allowed",
            ));
        }
        let existing_canonical = existing.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!(
                    "destination parent {} cannot be resolved: {error}",
                    existing.display()
                ),
            )
        })?;
        if !self.is_granted(&existing_canonical) {
            return Err(path_error(tool_name, raw, OUTSIDE_ROOTS));
        }
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| {
                ToolError::exec_failed(
                    tool_name,
                    format!(
                        "failed to create destination directory {}: {error}",
                        parent.display()
                    ),
                )
            })?;
        }
        Ok(candidate)
    }

    pub(crate) fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(self.root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn prepare_candidate(&self, raw: &str, tool_name: &str) -> Result<PathBuf, ToolError> {
        let raw_path = Path::new(raw);
        if raw.trim().is_empty() {
            return Err(path_error(tool_name, raw, "path must not be empty"));
        }
        // An absolute path is a containment ticket, not a free pass: the
        // candidate is taken as-is here, and `resolve_*` then requires its
        // canonical form to land inside a granted root. Parent traversal stays
        // banned even in absolute form — a `..` segment is exactly the tool
        // for wandering out of the root the prefix appeared to promise, and
        // the model can always write the resolved path instead.
        if raw_path.is_absolute() {
            if raw_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(path_error(
                    tool_name,
                    raw,
                    "parent traversal is not allowed",
                ));
            }
            return Ok(raw_path.to_path_buf());
        }
        if raw_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }) {
            return Err(path_error(
                tool_name,
                raw,
                "parent traversal and absolute prefixes are not allowed",
            ));
        }
        Ok(self.root().join(raw_path))
    }
}

/// Message for a path whose canonical form is not under any granted root. It
/// teaches the sanctioned remedies — the model's own `request_write_root`
/// request (user-approved) first, the user-typed `/add-dir` second — so a
/// model that meant to touch a sibling repo learns the grant channel instead
/// of retrying blind. Also the in-band marker `record_tool_result` uses to
/// classify the failure as a boundary denial (circuit breaker + cascade
/// exemption); producer and consumer share the constant so they cannot drift.
pub(crate) const OUTSIDE_ROOTS: &str = "path is outside every granted root (the workspace and \
--add-dir directories); if this directory is genuinely needed, request it with the \
request_write_root tool (the user will be asked), or the user can grant it with /add-dir";

/// The first secret store `canonical` overlaps, in either direction: the
/// candidate sitting inside one, or being an ancestor that would cover one.
/// Split out from [`WorkspacePolicy::resolve_grant_target`] so the rule itself
/// is testable without needing real credential directories on the host.
fn sensitive_overlap<'a>(canonical: &Path, secrets: &'a [PathBuf]) -> Option<&'a Path> {
    secrets
        .iter()
        .find(|secret| canonical.starts_with(secret) || secret.starts_with(canonical))
        .map(PathBuf::as_path)
}

pub(crate) fn invalid(name: impl Into<String>, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        name: name.into(),
        message: message.into(),
    }
}

pub(crate) fn json_string(value: impl serde::Serialize) -> String {
    serde_json::to_string_pretty(&value).expect("serializing tool output should not fail")
}

pub(crate) fn contains_symlink(path: &Path, skip: &[PathBuf]) -> std::io::Result<bool> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        // Only named segments can be symlinks. Prefix/RootDir must be
        // accumulated into `current` but not stat'd on their own: on Windows
        // `canonicalize` yields verbatim paths (`\\?\D:\...`) whose first
        // component is the bare disk prefix `\\?\D:`, and `symlink_metadata`
        // on it fails with ERROR_INVALID_FUNCTION (os error 1).
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        // Granted roots are canonical, so neither they nor their ancestors can
        // be symlinks; skipping them keeps the walk focused on the segments
        // the caller actually chose.
        if skip.contains(&current) {
            continue;
        }
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_error(tool_name: &str, raw: &str, message: &str) -> ToolError {
    invalid(tool_name, format!("invalid path '{raw}': {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        (dir, canonical)
    }

    #[test]
    fn contains_symlink_walks_canonical_path_without_error() {
        // `canonicalize` yields a verbatim `\\?\D:\...` path on Windows, whose
        // first component is the bare disk prefix. Statting it directly fails
        // with ERROR_INVALID_FUNCTION; the walk must skip Prefix/RootDir.
        let (_dir, root) = canonical_tempdir();
        let file = root.join("note.txt");
        fs::write(&file, "x").unwrap();
        assert!(!contains_symlink(&file, std::slice::from_ref(&root)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn contains_symlink_still_detects_a_symlink_segment() {
        let (_dir, root) = canonical_tempdir();
        let target = root.join("real");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(contains_symlink(&link.join("inner"), std::slice::from_ref(&root)).unwrap());
    }

    #[test]
    fn absolute_path_inside_primary_is_accepted() {
        let (_dir, root) = canonical_tempdir();
        fs::write(root.join("note.txt"), "x").unwrap();
        let policy = WorkspacePolicy::new(root.clone()).unwrap();
        let resolved = policy
            .resolve_existing(&root.join("note.txt").to_string_lossy(), "read_file")
            .unwrap();
        assert_eq!(resolved, root.join("note.txt"));
    }

    #[test]
    fn absolute_path_inside_extra_root_is_accepted() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        fs::write(extra.join("host.ts"), "x").unwrap();
        let policy =
            WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra.clone()])).unwrap();
        let resolved = policy
            .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
            .unwrap();
        assert_eq!(resolved, extra.join("host.ts"));
    }

    #[test]
    fn write_into_extra_root_creates_missing_parents_inside_it() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let policy =
            WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra.clone()])).unwrap();
        let target = extra.join("src/new_mod/thing.rs");
        let resolved = policy
            .resolve_for_write(&target.to_string_lossy(), "write_file")
            .unwrap();
        assert_eq!(resolved, target);
        assert!(extra.join("src/new_mod").is_dir());
    }

    #[test]
    fn absolute_path_outside_all_roots_is_rejected() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let (_c, outside) = canonical_tempdir();
        fs::write(outside.join("secret.txt"), "x").unwrap();
        let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra])).unwrap();
        let raw = outside.join("secret.txt");
        let read = policy.resolve_existing(&raw.to_string_lossy(), "read_file");
        assert!(read.is_err(), "read outside all roots must be rejected");
        let write = policy.resolve_for_write(&raw.to_string_lossy(), "write_file");
        assert!(write.is_err(), "write outside all roots must be rejected");
        // The rejection teaches the remedy: a model that meant to touch a
        // sibling repo must learn the grant channels, not retry blind.
        let message = write.unwrap_err().to_string();
        assert!(
            message.contains("/add-dir"),
            "rejection must name the grant channel: {message}"
        );
    }

    #[test]
    fn absolute_path_with_parent_traversal_is_rejected() {
        let (_dir, root) = canonical_tempdir();
        let policy = WorkspacePolicy::new(root.clone()).unwrap();
        // Canonically inside the root, but spelled with `..` — still refused.
        let sneaky = format!("{}/sub/../note.txt", root.display());
        assert!(policy.resolve_for_write(&sneaky, "write_file").is_err());
    }

    #[test]
    fn relative_paths_still_resolve_against_primary_only() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        fs::write(extra.join("only-here.txt"), "x").unwrap();
        let policy =
            WorkspacePolicy::new(WorkspaceRoots::new(primary.clone(), vec![extra])).unwrap();
        // The extra root never becomes a fallback base for relative paths;
        // it is addressable by absolute path alone.
        assert!(
            policy
                .resolve_existing("only-here.txt", "read_file")
                .is_err()
        );
        let resolved = policy.resolve_for_write("fresh.txt", "write_file").unwrap();
        assert_eq!(resolved, primary.join("fresh.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_segment_under_extra_root_is_rejected() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let (_c, outside) = canonical_tempdir();
        fs::write(outside.join("secret.txt"), "x").unwrap();
        let link = extra.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra])).unwrap();
        let raw = link.join("secret.txt");
        assert!(
            policy
                .resolve_existing(&raw.to_string_lossy(), "read_file")
                .is_err(),
            "symlinked segment under an extra root must be rejected"
        );
    }

    #[test]
    fn extras_are_deduped_and_primary_is_not_repeated() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let policy = WorkspacePolicy::new(WorkspaceRoots::new(
            primary.clone(),
            vec![extra.clone(), extra.clone(), primary.clone()],
        ))
        .unwrap();
        assert_eq!(policy.granted_roots(), &[primary, extra]);
    }

    #[test]
    fn missing_extra_root_fails_construction() {
        let (_a, primary) = canonical_tempdir();
        let missing = primary.join("does-not-exist");
        let result = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![missing]));
        assert!(result.is_err(), "an unresolvable grant must refuse launch");
    }

    /// The load-bearing property of the shared boundary: a grant lands in
    /// every clone taken BEFORE it — that is what lets a mid-session
    /// `request_write_root` reach tools registered at launch without any
    /// registry rebuild.
    #[test]
    fn grant_extra_is_visible_through_prior_clones() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        fs::write(extra.join("host.ts"), "x").unwrap();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        let tool_held_clone = policy.clone(); // what a registered tool holds
        assert!(
            tool_held_clone
                .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
                .is_err(),
            "not granted yet"
        );

        let outcome = policy.grant_extra(&extra).unwrap();
        assert!(matches!(
            outcome,
            RootGrantOutcome::Granted { ref canonical } if *canonical == extra
        ));
        assert_eq!(
            tool_held_clone.granted_roots(),
            vec![primary, extra.clone()]
        );
        assert!(
            tool_held_clone
                .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
                .is_ok(),
            "the clone taken before the grant must see it"
        );
    }

    #[test]
    fn grant_extra_reports_covered_paths_without_recording() {
        let (_a, primary) = canonical_tempdir();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        // Inside the primary (including the primary itself): already granted.
        let sub = primary.join("src");
        fs::create_dir(&sub).unwrap();
        for covered in [&primary, &sub] {
            assert!(
                matches!(
                    policy.grant_extra(covered).unwrap(),
                    RootGrantOutcome::AlreadyGranted { .. }
                ),
                "{} is covered",
                covered.display()
            );
        }
        assert_eq!(policy.granted_roots(), vec![primary], "nothing recorded");
    }

    #[test]
    fn grant_extra_fails_closed_on_bad_paths() {
        let (_a, primary) = canonical_tempdir();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        // Relative: ambiguous about its base — refused outright.
        assert!(policy.grant_extra(Path::new("relative/dir")).is_err());
        // Nonexistent: nothing to canonicalize.
        assert!(policy.grant_extra(&primary.join("nope-missing")).is_err());
        // A file is not a containment zone.
        let file = primary.join("f.txt");
        fs::write(&file, "x").unwrap();
        assert!(policy.grant_extra(&file).is_err());
        assert_eq!(policy.granted_roots(), vec![primary], "all refused");
    }

    /// The request channel refuses the home directory, every ancestor of it
    /// (each would cover the whole home), and the filesystem root — the tool
    /// description promises the model never to ask for those, and this makes
    /// the promise enforced rather than advisory. `--add-dir` stays the
    /// human's own call and is deliberately not subject to this floor.
    #[test]
    fn grant_extra_refuses_home_and_its_ancestors() {
        let (_a, primary) = canonical_tempdir();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
            eprintln!("no resolvable home dir on this host; skipping");
            return;
        };
        if home.starts_with(&primary) || primary.starts_with(&home) {
            // A workspace inside (or above) home would legitimately cover it.
            eprintln!("tempdir overlaps home on this host; skipping");
            return;
        }
        for target in home.ancestors() {
            assert!(
                policy.grant_extra(target).is_err(),
                "{} must be refused: it covers the home directory",
                target.display()
            );
        }
        assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
    }

    /// A directory whose name embeds control characters is refused before
    /// anyone is prompted: the panel could not display it faithfully (an
    /// embedded newline or escape byte fabricates panel lines), and a prompt
    /// the human cannot read is not an approval. The TUI additionally
    /// sanitizes what it renders — this is the fail-closed layer underneath.
    #[cfg(unix)]
    #[test]
    fn grant_extra_refuses_names_with_control_characters() {
        let (_a, primary) = canonical_tempdir();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        for name in ["evil\ndir", "evil\x1b[2Kdir"] {
            let evil = primary.join(name);
            fs::create_dir(&evil).unwrap();
            let Err(error) = policy.grant_extra(&evil) else {
                panic!("control characters in the name must refuse: {name:?}");
            };
            assert!(
                error.to_string().contains("control characters"),
                "the reason must name the problem: {error}"
            );
        }
        assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
    }

    /// The credential floor's rule, pinned without needing real secret
    /// directories on the host: overlap in EITHER direction is refused, and an
    /// unrelated sibling is not.
    #[test]
    fn sensitive_overlap_refuses_both_directions() {
        let secrets = [PathBuf::from("/home/u/.ssh"), PathBuf::from("/home/u/.aws")];
        // The candidate IS the store, or sits inside it.
        for inside in ["/home/u/.ssh", "/home/u/.ssh/keys"] {
            assert_eq!(
                sensitive_overlap(Path::new(inside), &secrets),
                Some(Path::new("/home/u/.ssh")),
                "{inside} overlaps a credential store"
            );
        }
        // The candidate is an ancestor that would cover a store.
        assert_eq!(
            sensitive_overlap(Path::new("/home/u"), &secrets),
            Some(Path::new("/home/u/.ssh"))
        );
        // Unrelated siblings stay grantable — the floor must not swallow
        // ordinary project directories.
        for outside in ["/home/u/projects", "/home/u/.sshfoo", "/srv/build"] {
            assert_eq!(
                sensitive_overlap(Path::new(outside), &secrets),
                None,
                "{outside} must remain grantable"
            );
        }
    }

    /// The wiring half: the shared list really does name the credential stores
    /// and deep-code's own config directory, so the rule above is applied to
    /// the paths that matter. Pinned separately from the sandbox's use of the
    /// same constant — that is what keeps the kernel fence and this fence from
    /// drifting apart.
    #[test]
    fn sensitive_paths_cover_the_credential_stores_and_deep_code_home() {
        let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
            eprintln!("no resolvable home dir on this host; skipping");
            return;
        };
        let secrets = crate::paths::sensitive_paths();
        for entry in [".ssh", ".aws", ".git-credentials", ".deep-code"] {
            assert!(
                secrets.contains(&home.join(entry)),
                "{entry} must be refused by the request channel: {secrets:?}"
            );
        }
    }

    /// End-to-end through the real resolver: deep-code's own config directory
    /// holds the plaintext API key and the only `auto_allow` layer that is
    /// honoured, and `read_file`/`write_file` never meet the sandbox that
    /// denies it — so the request channel must refuse it outright. Runs where
    /// that directory exists (any real install); skips otherwise.
    #[test]
    fn grant_extra_refuses_deep_code_home() {
        let (_a, primary) = canonical_tempdir();
        let policy = WorkspacePolicy::new(primary.clone()).unwrap();
        let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
            eprintln!("no resolvable home dir on this host; skipping");
            return;
        };
        let config_dir = home.join(crate::paths::DEEP_CODE_DIR);
        if !config_dir.is_dir() || primary.starts_with(&config_dir) {
            eprintln!("no ~/.deep-code on this host; skipping");
            return;
        }
        let Err(error) = policy.grant_extra(&config_dir) else {
            panic!("granting {} must be refused", config_dir.display());
        };
        assert!(
            error.to_string().contains("credential store"),
            "the reason must name the problem: {error}"
        );
        assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
    }

    /// A symlink to a directory canonicalizes to its target — the resolution
    /// step speaks only canonical paths, so the prompt displays the real
    /// target and the grant records that same value. (That prompt-vs-grant
    /// equality is enforced by the runtime's re-resolve-and-compare; pinned
    /// in the runtime integration tests.)
    #[cfg(unix)]
    #[test]
    fn grant_extra_grants_the_canonical_target_of_a_symlink() {
        let (_a, primary) = canonical_tempdir();
        let (_b, target) = canonical_tempdir();
        let link = primary.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let outcome = policy_grant(&primary, &link);
        assert!(
            matches!(outcome, RootGrantOutcome::Granted { ref canonical } if *canonical == target),
            "the grant must be the resolved target, not the link spelling"
        );
    }

    #[cfg(unix)]
    fn policy_grant(primary: &Path, requested: &Path) -> RootGrantOutcome {
        WorkspacePolicy::new(primary.to_path_buf())
            .unwrap()
            .grant_extra(requested)
            .unwrap()
    }
}
