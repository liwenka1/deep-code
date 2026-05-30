use std::fs;
use std::path::PathBuf;

use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolCall, ToolError, ToolRegistry, ToolResult, ToolSpec};
use crate::workspace_policy::{
    WorkspacePolicy, contains_symlink, invalid, json_string, optional_bool, optional_str,
    required_str,
};

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_GREP_RESULTS: usize = 100;
const MAX_GREP_RESULTS: usize = 500;
const DEFAULT_CONTEXT_LINES: usize = 2;

#[derive(Debug, Clone)]
pub struct WorkspaceTools {
    root: WorkspacePolicy,
}

impl WorkspaceTools {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspacePolicy::new(root)?,
        })
    }

    pub fn into_registry(self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ReadFileTool::new(self.root.clone()));
        registry.register(ListDirTool::new(self.root.clone()));
        registry.register(GrepFilesTool::new(self.root.clone()));
        registry.register(WriteFileTool::new(self.root.clone()));
        registry.register(ApplyPatchTool::new(self.root));
        registry
    }
}

pub fn workspace_tool_registry(root: impl Into<PathBuf>) -> Result<ToolRegistry, ToolError> {
    Ok(WorkspaceTools::new(root)?.into_registry())
}

#[derive(Debug, Clone)]
struct ReadFileTool {
    root: WorkspacePolicy,
}

impl ReadFileTool {
    const NAME: &'static str = "read_file";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read a UTF-8 file from the workspace. Supports start_line and max_lines for bounded reads.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "start_line": {"type": "integer", "description": "1-based line number, default 1"},
                    "max_lines": {"type": "integer", "description": "Maximum lines to return, default 200, max 500"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let path_arg = required_str(&call.arguments, "path", Self::NAME)?;
        let start_line = optional_usize(&call.arguments, "start_line", 1, Self::NAME)?;
        if start_line == 0 {
            return Err(invalid(Self::NAME, "start_line must be greater than 0"));
        }
        let max_lines =
            optional_usize(&call.arguments, "max_lines", DEFAULT_READ_LINES, Self::NAME)?
                .clamp(1, MAX_READ_LINES);
        let path = self.root.resolve_existing(path_arg, Self::NAME)?;
        let metadata = fs::metadata(&path).map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to read metadata for {}: {error}", path.display()),
        })?;
        if !metadata.is_file() {
            return Err(invalid(Self::NAME, "path is not a file"));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!(
                    "{} is larger than the current 2 MiB read limit",
                    self.root.relative_display(&path)
                ),
            });
        }
        let contents = fs::read_to_string(&path).map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to read {} as UTF-8: {error}", path.display()),
        })?;
        let lines = contents.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start_index = start_line.saturating_sub(1);
        let selected = lines
            .iter()
            .enumerate()
            .skip(start_index)
            .take(max_lines)
            .map(|(index, line)| json!({"line": index + 1, "text": line}))
            .collect::<Vec<_>>();
        let next_start_line = if start_index + selected.len() < total_lines {
            Some(start_index + selected.len() + 1)
        } else {
            None
        };
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "path": self.root.relative_display(&path),
                "total_lines": total_lines,
                "start_line": start_line,
                "max_lines": max_lines,
                "truncated": next_start_line.is_some(),
                "next_start_line": next_start_line,
                "lines": selected
            })),
        ))
    }
}

#[derive(Debug, Clone)]
struct ListDirTool {
    root: WorkspacePolicy,
}

impl ListDirTool {
    const NAME: &'static str = "list_dir";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for ListDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "List a workspace directory with structured entries.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path, default ."}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let path_arg = optional_str(&call.arguments, "path").unwrap_or(".");
        let path = self.root.resolve_existing(path_arg, Self::NAME)?;
        if !path.is_dir() {
            return Err(invalid(Self::NAME, "path is not a directory"));
        }
        let mut entries = fs::read_dir(&path)
            .map_err(|error| ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!("failed to list {}: {error}", path.display()),
            })?
            .map(|entry| {
                let entry = entry.map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
                    message: format!("failed to read directory entry: {error}"),
                })?;
                let file_type = entry
                    .file_type()
                    .map_err(|error| ToolError::ExecutionFailed {
                        name: Self::NAME.to_string(),
                        message: format!("failed to read entry type: {error}"),
                    })?;
                let kind = if file_type.is_dir() {
                    "directory"
                } else if file_type.is_file() {
                    "file"
                } else if file_type.is_symlink() {
                    "symlink"
                } else {
                    "other"
                };
                let metadata = entry.metadata().ok();
                Ok(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": self.root.relative_display(&entry.path()),
                    "kind": kind,
                    "size_bytes": metadata.as_ref().map(fs::Metadata::len),
                }))
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        entries.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "path": self.root.relative_display(&path),
                "entries": entries
            })),
        ))
    }
}

#[derive(Debug, Clone)]
struct GrepFilesTool {
    root: WorkspacePolicy,
}

impl GrepFilesTool {
    const NAME: &'static str = "grep_files";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for GrepFilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Search UTF-8 workspace files with a regex. Returns structured matches with file, line number, and context.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern"},
                    "path": {"type": "string", "description": "Workspace-relative file or directory, default ."},
                    "context_lines": {"type": "integer", "description": "Context lines before and after each match, default 2"},
                    "case_insensitive": {"type": "boolean", "description": "Case-insensitive search, default false"},
                    "max_results": {"type": "integer", "description": "Maximum matches, default 100, max 500"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let pattern = required_str(&call.arguments, "pattern", Self::NAME)?;
        let path_arg = optional_str(&call.arguments, "path").unwrap_or(".");
        let context_lines = optional_usize(
            &call.arguments,
            "context_lines",
            DEFAULT_CONTEXT_LINES,
            Self::NAME,
        )?;
        let case_insensitive =
            optional_bool(&call.arguments, "case_insensitive", false, Self::NAME)?;
        let max_results = optional_usize(
            &call.arguments,
            "max_results",
            DEFAULT_GREP_RESULTS,
            Self::NAME,
        )?
        .clamp(1, MAX_GREP_RESULTS);
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|error| invalid(Self::NAME, format!("invalid regex pattern: {error}")))?;
        let search_path = self.root.resolve_existing(path_arg, Self::NAME)?;
        let mut files_searched = 0usize;
        let mut matches = Vec::new();

        for entry in WalkBuilder::new(&search_path)
            .standard_filters(true)
            .follow_links(false)
            .build()
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if contains_symlink(path, Some(self.root.root())).unwrap_or(true) {
                continue;
            }
            let Ok(metadata) = fs::metadata(path) else {
                continue;
            };
            if metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(contents) = fs::read_to_string(path) else {
                continue;
            };
            files_searched += 1;
            let lines = contents.lines().collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                let before_start = index.saturating_sub(context_lines);
                let after_end = (index + context_lines + 1).min(lines.len());
                matches.push(json!({
                    "path": self.root.relative_display(path),
                    "line_number": index + 1,
                    "line": line,
                    "context_before": (before_start..index)
                        .map(|line_index| json!({"line": line_index + 1, "text": lines[line_index]}))
                        .collect::<Vec<_>>(),
                    "context_after": ((index + 1)..after_end)
                        .map(|line_index| json!({"line": line_index + 1, "text": lines[line_index]}))
                        .collect::<Vec<_>>(),
                }));
                if matches.len() >= max_results {
                    break;
                }
            }
            if matches.len() >= max_results {
                break;
            }
        }

        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "pattern": pattern,
                "path": self.root.relative_display(&search_path),
                "files_searched": files_searched,
                "truncated": matches.len() >= max_results,
                "matches": matches
            })),
        ))
    }
}

#[derive(Debug, Clone)]
struct WriteFileTool {
    root: WorkspacePolicy,
}

impl WriteFileTool {
    const NAME: &'static str = "write_file";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Create or overwrite a UTF-8 file inside the workspace. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "content": {"type": "string", "description": "Full file contents"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let path_arg = required_str(&call.arguments, "path", Self::NAME)?;
        let content = required_str(&call.arguments, "content", Self::NAME)?;
        let path = self.root.resolve_for_write(path_arg, Self::NAME)?;
        fs::write(&path, content).map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to write {}: {error}", path.display()),
        })?;
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "path": self.root.relative_display(&path),
                "bytes_written": content.len()
            })),
        ))
    }
}

#[derive(Debug, Clone)]
struct ApplyPatchTool {
    root: WorkspacePolicy,
}

impl ApplyPatchTool {
    const NAME: &'static str = "apply_patch";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Apply a simple text replacement patch to one workspace file. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "old": {"type": "string", "description": "Text to replace; must occur exactly once"},
                    "new": {"type": "string", "description": "Replacement text"}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let path_arg = required_str(&call.arguments, "path", Self::NAME)?;
        let old = required_str(&call.arguments, "old", Self::NAME)?;
        let new = required_str(&call.arguments, "new", Self::NAME)?;
        if old.is_empty() {
            return Err(invalid(Self::NAME, "old must not be empty"));
        }
        let path = self.root.resolve_existing(path_arg, Self::NAME)?;
        let contents = fs::read_to_string(&path).map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to read {} as UTF-8: {error}", path.display()),
        })?;
        let count = contents.matches(old).count();
        if count != 1 {
            return Err(invalid(
                Self::NAME,
                format!("old text must occur exactly once, found {count} occurrences"),
            ));
        }
        let updated = contents.replacen(old, new, 1);
        fs::write(&path, updated).map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to write {}: {error}", path.display()),
        })?;
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "path": self.root.relative_display(&path),
                "replacements": 1
            })),
        ))
    }
}

fn optional_usize(
    input: &Value,
    field: &str,
    default: usize,
    tool_name: &str,
) -> Result<usize, ToolError> {
    match input.get(field) {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                invalid(
                    tool_name,
                    format!("field '{field}' must be a positive integer"),
                )
            }),
        None => Ok(default),
    }
}

#[cfg(test)]
#[path = "workspace_tools/tests.rs"]
mod tests;
