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
        let canonical = root
            .canonicalize()
            .map_err(|error| ToolError::ExecutionFailed {
                name: "workspace".to_string(),
                message: format!(
                    "failed to resolve workspace root {}: {error}",
                    root.display()
                ),
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
            ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                message: format!("failed to inspect {}: {error}", candidate.display()),
            }
        })? {
            return Err(path_error(tool_name, raw, "symlinks are not allowed"));
        }
        let canonical = candidate
            .canonicalize()
            .map_err(|error| ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                message: format!("failed to resolve {}: {error}", candidate.display()),
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
                ToolError::ExecutionFailed {
                    name: tool_name.to_string(),
                    message: format!("failed to inspect {}: {error}", candidate.display()),
                }
            })? {
                return Err(path_error(
                    tool_name,
                    raw,
                    "symlinks in the destination path are not allowed",
                ));
            }
            let canonical =
                candidate
                    .canonicalize()
                    .map_err(|error| ToolError::ExecutionFailed {
                        name: tool_name.to_string(),
                        message: format!("failed to resolve {}: {error}", candidate.display()),
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
            ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                message: format!("failed to inspect {}: {error}", parent.display()),
            }
        })? {
            return Err(path_error(
                tool_name,
                raw,
                "symlinks in the destination path are not allowed",
            ));
        }
        let parent_canonical =
            parent
                .canonicalize()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: tool_name.to_string(),
                    message: format!(
                        "destination parent {} does not exist or cannot be resolved: {error}",
                        parent.display()
                    ),
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

pub(crate) fn required_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
    tool_name: &str,
) -> Result<&'a str, ToolError> {
    input
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(tool_name, format!("missing string field '{field}'")))
}

pub(crate) fn optional_str<'a>(input: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(serde_json::Value::as_str)
}

pub(crate) fn optional_bool(
    input: &serde_json::Value,
    field: &str,
    default: bool,
    tool_name: &str,
) -> Result<bool, ToolError> {
    match input.get(field) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid(tool_name, format!("field '{field}' must be a boolean"))),
        None => Ok(default),
    }
}

pub(crate) fn optional_u64(
    input: &serde_json::Value,
    field: &str,
    default: u64,
    tool_name: &str,
) -> Result<u64, ToolError> {
    match input.get(field) {
        Some(value) => value.as_u64().ok_or_else(|| {
            invalid(
                tool_name,
                format!("field '{field}' must be a positive integer"),
            )
        }),
        None => Ok(default),
    }
}

pub(crate) fn json_string(value: impl serde::Serialize) -> String {
    serde_json::to_string_pretty(&value).expect("serializing tool output should not fail")
}

pub(crate) fn truncate_string(value: String, max_chars: usize) -> (String, bool, usize) {
    let total = value.chars().count();
    if total <= max_chars {
        return (value, false, 0);
    }
    let truncated = value.chars().take(max_chars).collect::<String>();
    (truncated, true, total - max_chars)
}

pub(crate) fn contains_symlink(path: &Path, stop_at: Option<&Path>) -> std::io::Result<bool> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
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
