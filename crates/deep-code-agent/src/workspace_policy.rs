use std::fs;
use std::path::{Component, Path, PathBuf};

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

#[derive(Debug, Clone)]
pub(crate) struct WorkspacePolicy {
    /// Canonical granted roots; `roots[0]` is always the primary workspace,
    /// the rest are `--add-dir` grants (deduped, never equal to the primary).
    roots: Vec<PathBuf>,
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
        let mut canonical_roots = vec![canonical_primary];
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
            if !canonical_roots.contains(&canonical) {
                canonical_roots.push(canonical);
            }
        }
        Ok(Self {
            roots: canonical_roots,
        })
    }

    /// The primary workspace root (canonical).
    pub(crate) fn root(&self) -> &Path {
        &self.roots[0]
    }

    /// Every granted root (canonical), primary first. This exact list is what
    /// the OS sandbox turns into write grants, so tool-layer containment and
    /// kernel confinement cannot drift apart.
    pub(crate) fn granted_roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn is_granted(&self, canonical: &Path) -> bool {
        self.roots.iter().any(|root| canonical.starts_with(root))
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
        if contains_symlink(&candidate, &self.roots).map_err(|error| {
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
            if contains_symlink(&candidate, &self.roots).map_err(|error| {
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
        if contains_symlink(existing, &self.roots).map_err(|error| {
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
/// names both grant channels — the `/add-dir` command and the `--add-dir`
/// flag — so a model (or user) that meant to touch a sibling repo learns the
/// sanctioned way to get it granted instead of retrying blind. Also the
/// in-band marker `record_tool_result` uses to classify the failure as a
/// boundary denial (circuit breaker + cascade exemption); producer and
/// consumer share the constant so they cannot drift.
pub(crate) const OUTSIDE_ROOTS: &str = "path is outside every granted root (the workspace and \
--add-dir directories); if the user intends this path, ask them to grant it with the /add-dir \
command (or relaunch with --add-dir)";

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
}
