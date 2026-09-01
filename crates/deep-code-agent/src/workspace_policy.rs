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
                format!(
                    "failed to inspect {}: {error}",
                    self.relative_display(&candidate)
                ),
            )
        })? {
            return Err(path_error(tool_name, raw, "symlinks are not allowed"));
        }
        let canonical = crate::paths::canonicalize(&candidate).map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!(
                    "failed to resolve {}: {error}",
                    self.relative_display(&candidate)
                ),
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
                    format!(
                        "failed to inspect {}: {error}",
                        self.relative_display(&candidate)
                    ),
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
                    format!(
                        "failed to resolve {}: {error}",
                        self.relative_display(&candidate)
                    ),
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
                format!(
                    "failed to inspect {}: {error}",
                    self.relative_display(existing)
                ),
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
                    self.relative_display(existing)
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
                        // Workspace-relative, like every sibling error in this
                        // module — this one was the last to spell an absolute
                        // host path into a model-facing message.
                        self.relative_display(parent)
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
mod tests;
