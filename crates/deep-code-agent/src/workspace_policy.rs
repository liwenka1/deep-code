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
        let canonical_primary = crate::paths::canonicalize(&primary).map_err(|error| {
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
            let canonical = crate::paths::canonicalize(&extra).map_err(|error| {
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
        let canonical = crate::paths::canonicalize(raw).map_err(|error| {
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
        if !self.is_granted(&canonical)
            && let Some(reason) = refuse_as_unattended_root(&canonical)
        {
            return Err(invalid(
                TOOL,
                format!(
                    "refusing to grant '{}': {reason}; request the narrowest directory that \
                     unblocks the task",
                    canonical.display()
                ),
            ));
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
        let canonical = crate::paths::canonicalize(&candidate).map_err(|error| {
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
        // `symlink_metadata`, NOT `exists()`: the latter follows symlinks, so a
        // symlink whose target does not exist yet reported `false` and fell to
        // the non-existent branch below — which walks up from `parent` and
        // therefore never stats the leaf at all. The unresolved path was then
        // handed back and `fs::write` (`O_CREAT`, no `O_NOFOLLOW`) created the
        // link's target: `ln -s ~/.ssh/authorized_keys ws/notes.txt` followed by
        // `write_file notes.txt` wrote outside every granted root, with the
        // panel showing "new file notes.txt". Planting the link is an ordinary
        // permitted write inside a root, and these tools run in-process where no
        // sandbox sees them, so this bypassed both fences at once — including
        // the credential floor whose whole reason for existing is that
        // `read_file`/`write_file` never meet the kernel fence.
        //
        // A link that exists is a link whether or not its target does. Both
        // spellings now take this branch, where `contains_symlink` (which stats
        // with `symlink_metadata` too) refuses the leaf.
        if candidate.symlink_metadata().is_ok() {
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
            let canonical = crate::paths::canonicalize(&candidate).map_err(|error| {
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
        // Containment is decided on the deepest ancestor that actually exists —
        // canonicalized, and symlink-checked over the same existing portion —
        // so a symlinked or otherwise escaping ancestor still cannot be used
        // to write outside the granted roots. (A non-existent path can be
        // neither canonicalized nor stat'd, so the old code failed here rather
        // than at any deliberate check.)
        //
        // Resolution itself touches NOTHING on disk. It also runs at
        // preview/approval time, before any human decision, and a denied
        // write must leave no trace — missing parents used to be created
        // right here, which meant rendering the approval panel planted
        // directories the user then declined. Execution creates them via
        // [`Self::prepare_for_write`] after the decision.
        // `symlink_metadata`, not `exists()`: the latter FOLLOWS links, so a
        // dangling one answers "absent" and this walk climbs straight past the
        // very entry `contains_symlink` below is meant to judge. That is the
        // exact predicate, in this exact function, whose `exists()` spelling
        // 45 lines up is documented as a past write-through — safe here today
        // only because `create_dir_all` in `prepare_for_write` happens to
        // re-fail with EEXIST on a dangling link, which is an accident of
        // std's internals and nothing this file controls.
        let mut existing = parent;
        while existing.symlink_metadata().is_err() {
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
        let existing_canonical = crate::paths::canonicalize(existing).map_err(|error| {
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
        Ok(candidate)
    }

    /// [`Self::resolve_for_write`] plus the one side effect execution needs:
    /// missing parent directories are created rather than refused. A coding
    /// agent adds new modules constantly, and failing `write_file` on
    /// `src/new_mod/thing.rs` forced the model into a separate, separately
    /// approved `mkdir -p` for a directory the write tool was already
    /// authorized to create.
    ///
    /// Kept out of resolution itself so that preview/approval can resolve the
    /// same path without leaving anything on disk (see the note inside
    /// [`Self::resolve_for_write`]). Only the executing tool calls this.
    pub(crate) fn prepare_for_write(
        &self,
        raw: &str,
        tool_name: &str,
    ) -> Result<PathBuf, ToolError> {
        let candidate = self.resolve_for_write(raw, tool_name)?;
        // The disk can change between the resolve above and the mkdir below.
        // That same-instant window existed when the mkdir lived inside
        // resolution and is inherited, not widened, by the split; it is also
        // not closable from here — the missing segments could not be
        // canonicalized at check time precisely because they did not exist.
        if let Some(parent) = candidate.parent() {
            // No exists() pre-check: create_dir_all already succeeds on an
            // existing directory, and a gate here would only read like a
            // semantic guard while widening the race window above.
            fs::create_dir_all(parent).map_err(|error| {
                // Two failures reach here, and neither errno reads as its own
                // cause: a symlink planted or dangling among the segments
                // resolution vetted as merely missing, and an ordinary FILE
                // sitting where a directory has to go. Name whichever it is,
                // so the model learns the rule instead of retrying into word
                // salad.
                //
                // Not one shared errno, as this note used to claim. The
                // symlink is EEXIST(17); the file is EEXIST only when it
                // blocks at the FINAL segment of `parent`, and ENOTDIR(20)
                // whenever it blocks further up — which is the shape
                // `prepare_for_write_names_the_file_blocking_the_directory`
                // actually builds. Unreadable either way, which is the point.
                //
                // `unwrap_or(false)` on the probe: if the inspection itself
                // fails we do not know, and guessing "symlink" would be a
                // false accusation — the bare OS error is the honest fallback.
                let diagnosis = if contains_symlink(parent, &self.granted_roots()).unwrap_or(false)
                {
                    " (symlinks in the destination path are not allowed)"
                } else if first_file_segment(parent).is_some() {
                    " (a file already exists on that path, so the directory cannot be created)"
                } else {
                    ""
                };
                ToolError::exec_failed(
                    tool_name,
                    format!(
                        "failed to create destination directory {}: {error}{diagnosis}",
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

/// Why this canonical directory may not become a writable root through any
/// channel the human did not type — or `None` when it may.
///
/// Two channels are subject to it, and both for the same reason: nobody is
/// vouching for the path at the moment it takes effect.
///
/// 1. `request_write_root` — the model chooses the target, so a plausible
///    spelling is all it takes. Refused before anyone is prompted.
/// 2. A grant restored from a session record on resume. The record lives at
///    `<workspace>/.deep-code/sessions/<id>.json`, **inside the primary
///    writable root**, so it is an ordinary `write_file` target for the model;
///    and `-c` picks the newest record by an `updated_at_ms` read straight out
///    of that file. Without this floor every check above was bypassable by
///    writing a record with `extra_roots: ["/"]` and a large timestamp — the
///    approval prompt was not defeated, it was skipped.
///
/// `--add-dir` is deliberately NOT subject to it: that path is the human's own
/// command line, exercised knowingly, and refusing `--add-dir ~` would break a
/// legitimate (if unwise) choice. The distinction is authorship, not danger.
pub(crate) fn refuse_as_unattended_root(canonical: &Path) -> Option<String> {
    if canonical.parent().is_none() {
        return Some("it is the filesystem root".to_string());
    }
    if let Some(home) =
        crate::paths::home_dir().and_then(|home| crate::paths::canonicalize(&home).ok())
        && home.starts_with(canonical)
    {
        return Some("it would make the entire home directory writable".to_string());
    }
    // Credential stores and deep-code's own config directory. The OS sandbox
    // already denies writes to exactly these under every policy, precisely so
    // that a writable root cannot reach them — but `read_file`/`write_file` run
    // in-process and never meet the sandbox, so without this floor a grant
    // would hand over what the kernel fence refuses. `~/.deep-code` is the
    // worst of them: it holds the plaintext API key, and `auto_allow` is
    // honoured only from that file, so a write there outlives the session any
    // approval prompt scopes itself to.
    //
    // Overlap in EITHER direction is refused: the target may not be inside a
    // secret store, nor an ancestor that would cover one.
    if let Some(secret) = sensitive_overlap(canonical, &crate::paths::sensitive_paths()) {
        return Some(format!(
            "it overlaps the credential store '{}', which is never writable through an \
             unattended grant",
            secret.display()
        ));
    }
    None
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

/// Walk `path` yielding the accumulated prefix for every NAMED segment.
///
/// Prefix/RootDir are accumulated into the prefix but never yielded, because
/// they are not stattable paths on their own: these paths are canonical, so on
/// Windows the first component is a verbatim prefix (`\\?\C:`) whose
/// `symlink_metadata` fails with ERROR_INVALID_FUNCTION (os error 1).
///
/// One function because both walkers below need that rule and both learned it
/// separately — `first_file_segment` learned it the hard way, by shipping a
/// diagnosis that could never fire on Windows at all, forty lines under a
/// `contains_symlink` that already spelled the rule out. The next
/// component-walking function should not have to learn it a third time.
fn stattable_segments(path: &Path) -> impl Iterator<Item = (usize, PathBuf)> + '_ {
    let mut current = PathBuf::new();
    path.components()
        .enumerate()
        .filter_map(move |(index, component)| {
            current.push(component.as_os_str());
            matches!(component, Component::Normal(_)).then(|| (index, current.clone()))
        })
}

/// The first ancestor segment of `path` that exists and is NOT a directory —
/// the plain file blocking a `create_dir_all`. `None` when nothing on the path
/// is a non-directory, which is the ordinary case for every other failure.
fn first_file_segment(path: &Path) -> Option<PathBuf> {
    for (_, current) in stattable_segments(path) {
        match fs::symlink_metadata(&current) {
            Ok(meta) if !meta.is_dir() => return Some(current),
            // Missing from here down: nothing left to block anything.
            Err(_) => return None,
            Ok(_) => {}
        }
    }
    None
}

/// `skip` entries must be CANONICAL directories (they are the granted roots):
/// that is what licenses not stat'ing them or their ancestors at all.
pub(crate) fn contains_symlink(path: &Path, skip: &[PathBuf]) -> std::io::Result<bool> {
    // Fast-forward past the deepest skip root covering `path`. Canonical
    // roots cannot be symlinks and neither can their ancestors, yet the old
    // exact-match skip still lstat'd every segment ABOVE the root — grep
    // calls this once per file, so a workspace five directories deep paid
    // root-depth × file-count pure-waste syscalls per search. Segments at or
    // above the covering root are skipped without touching the disk; every
    // segment below it — the ones the caller actually chose — is checked
    // exactly as before.
    let start = skip
        .iter()
        .filter(|root| path.starts_with(root))
        .map(|root| root.components().count())
        .max()
        .unwrap_or(0);
    for (index, current) in stattable_segments(path) {
        if index < start {
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

    // Skip/triage policy (unix bugs panic, Windows may lack the privilege,
    // DEEPCODE_REQUIRE_SYMLINKS hardens CI) lives in `crate::test_symlinks`.
    use crate::test_symlinks::symlink_dir_for_test;

    #[test]
    fn contains_symlink_still_detects_a_symlink_segment() {
        let (_dir, root) = canonical_tempdir();
        let target = root.join("real");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        if !symlink_dir_for_test(&target, &link) {
            return;
        }
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
    fn prepare_for_write_creates_missing_parents_inside_extra_root() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let policy =
            WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra.clone()])).unwrap();
        let target = extra.join("src/new_mod/thing.rs");
        let resolved = policy
            .prepare_for_write(&target.to_string_lossy(), "write_file")
            .unwrap();
        assert_eq!(resolved, target);
        assert!(extra.join("src/new_mod").is_dir());
    }

    /// The `starts_with` filter on the fast-forward is load-bearing and had
    /// nothing pinning it: deleting it — so `start` becomes the deepest root
    /// granted, covering this path or not — left the whole lib suite green
    /// while making `contains_symlink` skip every segment under a SHALLOWER
    /// root. With `--add-dir` granting something deeper than the primary
    /// workspace, that is the boundary silently ceasing to detect symlinks in
    /// the workspace itself.
    ///
    /// The off-by-one direction was already covered (`start + 1` fails three
    /// tests); this is the other axis, and it needs two roots at different
    /// depths to show up at all.
    #[test]
    fn contains_symlink_still_checks_a_shallow_root_when_a_deeper_one_is_granted() {
        let (_a, primary) = canonical_tempdir();
        let (_b, base) = canonical_tempdir();
        // An extra root several levels deeper than the primary, as `--add-dir`
        // into a nested project directory would produce.
        let deeper = base.join("a/b/c/d");
        fs::create_dir_all(&deeper).unwrap();
        let deeper = deeper.canonicalize().unwrap();

        let target = primary.join("real");
        fs::create_dir(&target).unwrap();
        let link = primary.join("link");
        if !symlink_dir_for_test(&target, &link) {
            return;
        }

        let roots = [primary, deeper];
        assert!(
            contains_symlink(&link.join("inner"), &roots).unwrap(),
            "a symlink under the shallow root must still be caught while a deeper root is granted"
        );
    }

    /// A dangling symlink posing as a missing PARENT segment.
    ///
    /// It used to get past resolution entirely — the ancestor walk asked
    /// `exists()`, which FOLLOWS links, so a dangling one read as "absent" and
    /// the walk climbed straight past it — and trip only inside
    /// `create_dir_all`, as a bare "File exists" for a directory the tool was
    /// told to create. Both halves now refuse it: the walk asks
    /// `symlink_metadata` and stops AT the link, so `contains_symlink` judges
    /// it at resolve time, and the prepare-side diagnosis stays as the second
    /// line of defence for a link planted after resolution. Either way the
    /// failure names the rule, or the model retries into word salad instead of
    /// learning it.
    #[test]
    fn prepare_for_write_names_the_symlink_when_mkdir_hits_one() {
        let (_dir, root) = canonical_tempdir();
        let (_out, outside) = canonical_tempdir();
        let policy = WorkspacePolicy::new(root.clone()).unwrap();
        if !symlink_dir_for_test(&outside.join("gone"), &root.join("src")) {
            return;
        }
        let error = policy
            .prepare_for_write("src/new_mod/thing.rs", "write_file")
            .expect_err("a symlinked parent segment must fail the write");
        let message = format!("{error:?}");
        assert!(
            message.contains("symlinks in the destination path are not allowed"),
            "the failure must teach the rule: {message}"
        );
    }

    /// The OTHER errno-17 cause: an ordinary file sitting where a directory
    /// has to go. `create_dir_all` reports it as the same bare "File exists"
    /// the symlink case produces, and the two need completely different fixes,
    /// so the message has to say which one happened.
    #[test]
    fn prepare_for_write_names_the_file_blocking_the_directory() {
        let (_dir, root) = canonical_tempdir();
        let policy = WorkspacePolicy::new(root.clone()).unwrap();
        fs::write(root.join("src"), "not a directory").unwrap();
        let error = policy
            .prepare_for_write("src/new_mod/thing.rs", "write_file")
            .expect_err("a file blocking the destination directory must fail the write");
        let message = format!("{error:?}");
        assert!(
            message.contains("a file already exists on that path"),
            "the failure must name the blocker, not just echo the errno: {message}"
        );
        assert!(
            !message.contains("symlinks in the destination path"),
            "a plain file must not be reported as a symlink: {message}"
        );
    }

    /// Resolution must be a pure question. It runs at preview/approval time,
    /// before the human decides, and a denied write must leave no trace —
    /// creating `src/new_mod` while merely RENDERING the approval panel is a
    /// side effect the user never consented to. (Execution creates parents
    /// via `prepare_for_write`, pinned by the test above.)
    #[test]
    fn resolve_for_write_leaves_the_disk_untouched() {
        let (_dir, root) = canonical_tempdir();
        let policy = WorkspacePolicy::new(root.clone()).unwrap();
        let resolved = policy
            .resolve_for_write("src/new_mod/thing.rs", "write_file")
            .unwrap();
        assert_eq!(resolved, root.join("src/new_mod/thing.rs"));
        assert!(
            !root.join("src").exists(),
            "resolving a write must not create its parent directories"
        );
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

    #[test]
    fn symlink_segment_under_extra_root_is_rejected() {
        let (_a, primary) = canonical_tempdir();
        let (_b, extra) = canonical_tempdir();
        let (_c, outside) = canonical_tempdir();
        fs::write(outside.join("secret.txt"), "x").unwrap();
        let link = extra.join("link");
        if !symlink_dir_for_test(&outside, &link) {
            return;
        }
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
        // Enumerated from the list itself, plus deep-code's own directory, so
        // that adding an entry to `CREDENTIAL_ENTRIES` is pinned by this test
        // automatically. A hand-written subset was not a guard: it named four
        // entries, so a new one could be added to the list and dropped from
        // `sensitive_paths` without anything going red.
        for entry in crate::paths::CREDENTIAL_ENTRIES
            .iter()
            .copied()
            .chain(std::iter::once(crate::paths::DEEP_CODE_DIR))
        {
            assert!(
                secrets.contains(&home.join(entry)),
                "{entry} must be refused by the request channel: {secrets:?}"
            );
        }
        // The cloud trio in particular: `.aws` alone was the inconsistency —
        // GCP and Azure credentials are the same category and the same risk.
        for entry in [".aws", ".config/gcloud", ".azure"] {
            assert!(
                secrets.contains(&home.join(entry)),
                "{entry} must be covered: {secrets:?}"
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

    /// macOS gives the home directory two canonical spellings for one inode —
    /// `/Users/x` and the firmlinked `/System/Volumes/Data/Users/x` — and
    /// `realpath(3)` collapses neither, so each resolves to itself. Every
    /// floor here is a `starts_with` on canonical paths, so the Data spelling
    /// used to walk through all of them: not "inside home", not "overlapping
    /// a credential store", not `~/.deep-code` — while a write through it
    /// lands on exactly those files. Seatbelt is no backstop, because
    /// `read_file`/`write_file` are in-process and never meet it.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_firmlink_spelling_resolves_into_the_namespace_the_floors_use() {
        let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
            eprintln!("no resolvable home dir on this host; skipping");
            return;
        };
        let data_home = Path::new("/System/Volumes/Data").join(home.strip_prefix("/").unwrap());
        if !data_home.is_dir() {
            eprintln!("no firmlinked data volume on this host; skipping");
            return;
        }

        // The normalization itself: both spellings must land on one path, or
        // no prefix-based floor downstream can be sound.
        assert_eq!(
            crate::paths::canonicalize(&data_home).unwrap(),
            home,
            "the firmlink spelling must resolve into the same namespace as home"
        );

        // And the floor that consumes it. Home itself is always present; the
        // credential entries are only asserted where the host has them.
        let mut checked = vec![data_home.clone()];
        for entry in [".ssh", crate::paths::DEEP_CODE_DIR] {
            if data_home.join(entry).is_dir() {
                checked.push(data_home.join(entry));
            }
        }
        for candidate in checked {
            let canonical = crate::paths::canonicalize(&candidate).unwrap();
            assert!(
                refuse_as_unattended_root(&canonical).is_some(),
                "the firmlink spelling {} walked through the floor",
                candidate.display()
            );
        }
    }

    /// A symlink to a directory canonicalizes to its target — the resolution
    /// step speaks only canonical paths, so the prompt displays the real
    /// target and the grant records that same value. (That prompt-vs-grant
    /// equality is enforced by the runtime's re-resolve-and-compare; pinned
    /// in the runtime integration tests.)
    #[test]
    fn grant_extra_grants_the_canonical_target_of_a_symlink() {
        let (_a, primary) = canonical_tempdir();
        let (_b, target) = canonical_tempdir();
        let link = primary.join("link");
        if !symlink_dir_for_test(&target, &link) {
            return;
        }
        let outcome = policy_grant(&primary, &link);
        assert!(
            matches!(outcome, RootGrantOutcome::Granted { ref canonical } if *canonical == target),
            "the grant must be the resolved target, not the link spelling"
        );
    }

    fn policy_grant(primary: &Path, requested: &Path) -> RootGrantOutcome {
        WorkspacePolicy::new(primary.to_path_buf())
            .unwrap()
            .grant_extra(requested)
            .unwrap()
    }
}
