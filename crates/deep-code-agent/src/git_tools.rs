use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

use crate::workspace_policy::{
    WorkspacePolicy, invalid, json_string, optional_bool, optional_str, optional_u64,
    truncate_string,
};
use crate::tool::{Tool, ToolCall, ToolError, ToolRegistry, ToolResult, ToolSpec};

const MAX_OUTPUT_CHARS: usize = 40_000;
const DEFAULT_LOG_COUNT: u64 = 20;
const MAX_LOG_COUNT: u64 = 200;
const DEFAULT_UNIFIED: u64 = 3;
const MAX_UNIFIED: u64 = 50;

#[derive(Debug, Clone)]
pub struct GitTools {
    root: WorkspacePolicy,
}

impl GitTools {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspacePolicy::new(root)?,
        })
    }

    pub fn into_registry(self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(GitStatusTool::new(self.root.clone()));
        registry.register(GitDiffTool::new(self.root.clone()));
        registry.register(GitLogTool::new(self.root));
        registry
    }
}

pub fn git_tool_registry(root: impl Into<std::path::PathBuf>) -> Result<ToolRegistry, ToolError> {
    Ok(GitTools::new(root)?.into_registry())
}

#[derive(Debug, Clone)]
struct GitStatusTool {
    root: WorkspacePolicy,
}

impl GitStatusTool {
    const NAME: &'static str = "git_status";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for GitStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read git status with `git status --porcelain=v1 -b`, optionally scoped to a workspace path.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional workspace-relative path"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let ctx = GitContext::resolve(
            &self.root,
            optional_str(&call.arguments, "path"),
            Self::NAME,
        )?;
        let mut args = vec![
            "status".to_string(),
            "--porcelain=v1".to_string(),
            "-b".to_string(),
        ];
        push_pathspec(&mut args, &ctx);
        git_result(call, Self::NAME, &ctx, args, |stdout| {
            let (branch, entries) = parse_status_output(&stdout);
            json!({
                "branch": branch,
                "entries": entries,
                "status_output": stdout
            })
        })
    }
}

#[derive(Debug, Clone)]
struct GitDiffTool {
    root: WorkspacePolicy,
}

impl GitDiffTool {
    const NAME: &'static str = "git_diff";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for GitDiffTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read git diff output with safe defaults, optionally staged or scoped to a workspace path.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional workspace-relative path"},
                    "cached": {"type": "boolean", "description": "When true, read staged diff"},
                    "unified": {"type": "integer", "description": "Context lines, default 3, max 50"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let ctx = GitContext::resolve(
            &self.root,
            optional_str(&call.arguments, "path"),
            Self::NAME,
        )?;
        let cached = optional_bool(&call.arguments, "cached", false, Self::NAME)?;
        let unified =
            optional_u64(&call.arguments, "unified", DEFAULT_UNIFIED, Self::NAME)?.min(MAX_UNIFIED);
        let mut args = vec![
            "diff".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
            format!("--unified={unified}"),
        ];
        if cached {
            args.push("--cached".to_string());
        }
        push_pathspec(&mut args, &ctx);
        git_result(
            call,
            Self::NAME,
            &ctx,
            args,
            |stdout| json!({ "diff": stdout, "cached": cached, "unified": unified }),
        )
    }
}

#[derive(Debug, Clone)]
struct GitLogTool {
    root: WorkspacePolicy,
}

impl GitLogTool {
    const NAME: &'static str = "git_log";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }
}

impl Tool for GitLogTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read recent git history with `git log`, optionally scoped to a workspace path.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional workspace-relative path"},
                    "max_count": {"type": "integer", "description": "Maximum commits, default 20, max 200"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let ctx = GitContext::resolve(
            &self.root,
            optional_str(&call.arguments, "path"),
            Self::NAME,
        )?;
        let max_count = optional_u64(&call.arguments, "max_count", DEFAULT_LOG_COUNT, Self::NAME)?
            .clamp(1, MAX_LOG_COUNT);
        let mut args = vec![
            "log".to_string(),
            "--no-color".to_string(),
            format!("--max-count={max_count}"),
            "--date=iso-strict".to_string(),
            "--pretty=format:%H%nAuthor: %an <%ae>%nDate: %ad%nSubject: %s%n".to_string(),
        ];
        push_pathspec(&mut args, &ctx);
        git_result(
            call,
            Self::NAME,
            &ctx,
            args,
            |stdout| json!({ "log": stdout, "max_count": max_count }),
        )
    }
}

#[derive(Debug)]
struct GitContext {
    cwd: std::path::PathBuf,
    cwd_display: String,
    pathspec: Option<String>,
}

impl GitContext {
    fn resolve(
        root: &WorkspacePolicy,
        path: Option<&str>,
        tool_name: &str,
    ) -> Result<Self, ToolError> {
        let Some(path) = path else {
            return Ok(Self {
                cwd: root.root().to_path_buf(),
                cwd_display: ".".to_string(),
                pathspec: None,
            });
        };
        let resolved = root.resolve_existing(path, tool_name)?;
        let metadata =
            std::fs::metadata(&resolved).map_err(|error| ToolError::ExecutionFailed {
                name: tool_name.to_string(),
                message: format!("failed to inspect {}: {error}", resolved.display()),
            })?;
        if metadata.is_dir() {
            Ok(Self {
                cwd_display: root.relative_display(&resolved),
                cwd: resolved,
                pathspec: Some(".".to_string()),
            })
        } else if metadata.is_file() {
            let parent = resolved
                .parent()
                .ok_or_else(|| invalid(tool_name, "path has no parent directory"))?;
            let pathspec = resolved
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid(tool_name, "path is not valid UTF-8"))?
                .to_string();
            Ok(Self {
                cwd_display: root.relative_display(parent),
                cwd: parent.to_path_buf(),
                pathspec: Some(pathspec),
            })
        } else {
            Err(invalid(tool_name, "path must be a file or directory"))
        }
    }
}

fn push_pathspec(args: &mut Vec<String>, ctx: &GitContext) {
    if let Some(pathspec) = &ctx.pathspec {
        args.push("--".to_string());
        args.push(pathspec.clone());
    }
}

fn git_result(
    call: &ToolCall,
    tool_name: &str,
    ctx: &GitContext,
    args: Vec<String>,
    payload: impl FnOnce(String) -> Value,
) -> Result<ToolResult, ToolError> {
    let command = format!("git {}", args.join(" "));
    let output = run_git(&ctx.cwd, &args, tool_name)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "command": command,
                "cwd": ctx.cwd_display,
                "pathspec": ctx.pathspec,
                "tool_status": "error",
                "exit_code": output.status.code(),
                "stderr": stderr
            })),
        ));
    }
    let (stdout, truncated, omitted_chars) = truncate_string(stdout, MAX_OUTPUT_CHARS);
    let mut payload = payload(stdout);
    if let Some(object) = payload.as_object_mut() {
        object.insert("command".to_string(), json!(command));
        object.insert("cwd".to_string(), json!(ctx.cwd_display));
        object.insert("pathspec".to_string(), json!(ctx.pathspec));
        object.insert("tool_status".to_string(), json!("success"));
        object.insert("truncated".to_string(), json!(truncated));
        object.insert("omitted_chars".to_string(), json!(omitted_chars));
    }
    Ok(ToolResult::success(
        call.id.clone(),
        call.name.clone(),
        json_string(payload),
    ))
}

fn run_git(cwd: &std::path::Path, args: &[String], tool_name: &str) -> Result<Output, ToolError> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| ToolError::ExecutionFailed {
            name: tool_name.to_string(),
            message: format!("failed to run git: {error}"),
        })
}

fn parse_status_output(output: &str) -> (Option<String>, Vec<Value>) {
    let mut branch = None;
    let mut entries = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            branch = Some(rest.to_string());
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let xy = &line[..2];
        entries.push(json!({
            "xy": xy,
            "index": &xy[..1],
            "worktree": &xy[1..],
            "path": &line[3..]
        }));
    }
    (branch, entries)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::tool::ToolRunOutcome;

    fn workspace_tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("deep-code-git-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap()
    }

    fn workspace_root() -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    fn call(root: &std::path::Path, name: &str, arguments: Value) -> Value {
        let registry = git_tool_registry(root.to_path_buf()).unwrap();
        let call = ToolCall::new("call_1", name, arguments);
        let ToolRunOutcome::Result { result } = registry.run_tool_call(call, None).unwrap() else {
            panic!("expected result");
        };
        serde_json::from_str(&result.content).unwrap()
    }

    #[test]
    fn git_status_reports_changes() {
        let tmp = workspace_tempdir();
        fs::write(tmp.path().join("new.txt"), "new\n").unwrap();
        let rel = tmp
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap();
        let output = call(&workspace_root(), "git_status", json!({"path": rel}));
        assert!(output["status_output"].as_str().unwrap().contains("??"));
        assert_eq!(output["entries"][0]["xy"], "??");
    }

    #[test]
    fn git_diff_returns_structured_output() {
        let output = call(&workspace_root(), "git_diff", json!({"path": "."}));
        assert_eq!(output["tool_status"], "success");
        assert!(output["diff"].is_string());
    }

    #[test]
    fn git_log_reports_commit_subject() {
        let output = call(&workspace_root(), "git_log", json!({"max_count": 1}));
        assert!(output["log"].as_str().unwrap().contains("Subject:"));
    }

    #[test]
    fn git_rejects_path_escape() {
        let registry = git_tool_registry(workspace_root()).unwrap();
        let call = ToolCall::new("call_1", "git_status", json!({"path": "../outside"}));
        assert!(matches!(
            registry.run_tool_call(call, None),
            Err(ToolError::InvalidArguments { .. })
        ));
    }
}
