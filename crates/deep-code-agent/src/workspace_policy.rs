use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::tool::ToolError;

#[derive(Debug, Clone)]
pub(crate) struct WorkspacePolicy {
    root: PathBuf,
}

impl WorkspacePolicy {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let root = root.into();
        let canonical = root.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                "workspace",
                format!(
                    "failed to resolve workspace root {}: {error}",
                    root.display()
                ),
            )
        })?;
        Ok(Self { root: canonical })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn resolve_cwd(
        &self,
        raw: Option<&str>,
        tool_name: &str,
    ) -> Result<PathBuf, ToolError> {
        let Some(raw) = raw else {
            return Ok(self.root.clone());
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
        if contains_symlink(&candidate, Some(&self.root)).map_err(|error| {
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
        if !canonical.starts_with(&self.root) {
            return Err(path_error(tool_name, raw, "path escapes the workspace"));
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
            if contains_symlink(&candidate, Some(&self.root)).map_err(|error| {
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
            if !canonical.starts_with(&self.root) {
                return Err(path_error(tool_name, raw, "path escapes the workspace"));
            }
            return Ok(candidate);
        }
        let parent = candidate.parent().ok_or_else(|| {
            path_error(
                tool_name,
                raw,
                "path must have a parent directory inside workspace",
            )
        })?;
        if contains_symlink(parent, Some(&self.root)).map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!("failed to inspect {}: {error}", parent.display()),
            )
        })? {
            return Err(path_error(
                tool_name,
                raw,
                "symlinks in the destination path are not allowed",
            ));
        }
        let parent_canonical = parent.canonicalize().map_err(|error| {
            ToolError::exec_failed(
                tool_name,
                format!(
                    "destination parent {} does not exist or cannot be resolved: {error}",
                    parent.display()
                ),
            )
        })?;
        if !parent_canonical.starts_with(&self.root) {
            return Err(path_error(tool_name, raw, "path escapes the workspace"));
        }
        Ok(candidate)
    }

    pub(crate) fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn prepare_candidate(&self, raw: &str, tool_name: &str) -> Result<PathBuf, ToolError> {
        let raw_path = Path::new(raw);
        if raw.trim().is_empty() {
            return Err(path_error(tool_name, raw, "path must not be empty"));
        }
        if raw_path.is_absolute() {
            return Err(path_error(
                tool_name,
                raw,
                "absolute paths are not allowed; use a workspace-relative path",
            ));
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
        Ok(self.root.join(raw_path))
    }
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

pub(crate) fn contains_symlink(path: &Path, stop_at: Option<&Path>) -> std::io::Result<bool> {
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
        if stop_at.is_some_and(|root| current == root) {
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

    #[test]
    fn contains_symlink_walks_canonical_path_without_error() {
        // `canonicalize` yields a verbatim `\\?\D:\...` path on Windows, whose
        // first component is the bare disk prefix. Statting it directly fails
        // with ERROR_INVALID_FUNCTION; the walk must skip Prefix/RootDir.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("note.txt");
        fs::write(&file, "x").unwrap();
        assert!(!contains_symlink(&file, Some(&root)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn contains_symlink_still_detects_a_symlink_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("real");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(contains_symlink(&link.join("inner"), Some(&root)).unwrap());
    }
}
