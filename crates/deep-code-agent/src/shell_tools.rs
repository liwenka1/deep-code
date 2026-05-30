mod jobs;

use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::sandbox::SandboxManager;
use crate::tool::{Tool, ToolCall, ToolError, ToolRegistry, ToolResult, ToolSpec};
use crate::tool_execution::current_sandbox_policy;
use crate::workspace_policy::{
    WorkspacePolicy, invalid, json_string, optional_str, optional_u64, required_str,
};
pub use jobs::{BackgroundJobSummary, JobStore};
use jobs::{
    JobKind, JobState, JobStatus, SharedBuffer, cancel_job, command_output_json, job_snapshot_json,
    refresh_job, spawn_buffer_reader,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_OUTPUT_CHARS: usize = 20_000;
const DEFAULT_TAIL_CHARS: usize = 4_000;
const MAX_TAIL_CHARS: usize = 20_000;
const SHELL_RUN_STARTUP_WAIT_MS: u64 = 100;

#[derive(Debug, Clone)]
pub struct ShellTools {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
}

impl ShellTools {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspacePolicy::new(root)?,
            jobs: JobStore::default(),
            sandbox: SandboxManager::new(),
        })
    }

    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SandboxManager) -> Self {
        self.sandbox = sandbox;
        self
    }

    #[must_use]
    pub fn job_store(&self) -> JobStore {
        self.jobs.clone()
    }

    pub fn into_registry(self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ShellRunTool::new(
            self.root.clone(),
            self.jobs.clone(),
            self.sandbox.clone(),
        ));
        registry.register(JobStartTool::new(
            self.root.clone(),
            self.jobs.clone(),
            self.sandbox.clone(),
        ));
        registry.register(JobStatusTool::new(self.jobs.clone()));
        registry.register(JobTailTool::new(self.jobs.clone()));
        registry.register(JobCancelTool::new(self.jobs));
        registry
    }
}

pub fn shell_tool_registry(
    root: impl Into<std::path::PathBuf>,
) -> Result<(ToolRegistry, JobStore), ToolError> {
    let shell = ShellTools::new(root)?;
    let jobs = shell.job_store();
    Ok((shell.into_registry(), jobs))
}

#[derive(Debug, Clone)]
struct ShellRunTool {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
}

impl ShellRunTool {
    const NAME: &'static str = "shell_run";

    fn new(root: WorkspacePolicy, jobs: JobStore, sandbox: SandboxManager) -> Self {
        Self {
            root,
            jobs,
            sandbox,
        }
    }
}

impl Tool for ShellRunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Run a foreground shell command inside the workspace with timeout, bounded output, and a cancellable job record. Requires approval because shell commands can modify the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute"},
                    "cwd": {"type": "string", "description": "Optional workspace-relative working directory"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds, default 30000, max 300000"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let command = required_str(&call.arguments, "command", Self::NAME)?;
        if command.trim().is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self
            .root
            .resolve_cwd(optional_str(&call.arguments, "cwd"), Self::NAME)?;
        let timeout_ms = optional_u64(
            &call.arguments,
            "timeout_ms",
            DEFAULT_TIMEOUT_MS,
            Self::NAME,
        )?
        .clamp(1, MAX_TIMEOUT_MS);

        let started = Instant::now();
        let mut child = self
            .sandbox
            .wrap_shell_command(command, &cwd, self.root.root(), &current_sandbox_policy())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!("failed to start command: {error}"),
            })?;

        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        if let Some(pipe) = child.stdout.take() {
            spawn_buffer_reader(pipe, stdout.clone());
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_buffer_reader(pipe, stderr.clone());
        }
        let job_id = self.jobs.insert(JobState {
            kind: JobKind::Foreground,
            command: command.to_string(),
            cwd: self.root.relative_display(&cwd),
            started_at: started,
            timeout_deadline: Some(started + Duration::from_millis(timeout_ms)),
            status: JobStatus::Running,
            exit_code: None,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            child: Some(child),
        });
        let foreground_wait_deadline =
            Instant::now() + Duration::from_millis(SHELL_RUN_STARTUP_WAIT_MS.min(timeout_ms));

        loop {
            {
                let job = self.jobs.get(&job_id, Self::NAME)?;
                let mut job = job.lock().expect("job lock poisoned");
                refresh_job(&mut job);
                if job.status != JobStatus::Running {
                    break;
                }
                if Instant::now() >= foreground_wait_deadline {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }

        let job = self.jobs.get(&job_id, Self::NAME)?;
        let job = job.lock().expect("job lock poisoned");
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            command_output_json(&job_id, &job, MAX_OUTPUT_CHARS),
        ))
    }
}

#[derive(Debug, Clone)]
struct JobStartTool {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
}

impl JobStartTool {
    const NAME: &'static str = "job_start";

    fn new(root: WorkspacePolicy, jobs: JobStore, sandbox: SandboxManager) -> Self {
        Self {
            root,
            jobs,
            sandbox,
        }
    }
}

impl Tool for JobStartTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Start a background shell command inside the workspace. Requires approval because shell commands can modify the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Optional workspace-relative working directory"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let command = required_str(&call.arguments, "command", Self::NAME)?;
        if command.trim().is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self
            .root
            .resolve_cwd(optional_str(&call.arguments, "cwd"), Self::NAME)?;
        let mut child = self
            .sandbox
            .wrap_shell_command(command, &cwd, self.root.root(), &current_sandbox_policy())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!("failed to start background command: {error}"),
            })?;
        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        if let Some(pipe) = child.stdout.take() {
            spawn_buffer_reader(pipe, stdout.clone());
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_buffer_reader(pipe, stderr.clone());
        }

        let job_id = self.jobs.insert(JobState {
            kind: JobKind::Background,
            command: command.to_string(),
            cwd: self.root.relative_display(&cwd),
            started_at: Instant::now(),
            timeout_deadline: None,
            status: JobStatus::Running,
            exit_code: None,
            stdout,
            stderr,
            child: Some(child),
        });

        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            json_string(json!({
                "job_id": job_id,
                "command": command,
                "cwd": self.root.relative_display(&cwd),
                "status": "running",
                "kind": "background",
                "approval_reason": "shell commands can modify files, run code, or access the network"
            })),
        ))
    }
}

#[derive(Debug, Clone)]
struct JobStatusTool {
    jobs: JobStore,
}

impl JobStatusTool {
    const NAME: &'static str = "job_status";

    fn new(jobs: JobStore) -> Self {
        Self { jobs }
    }
}

impl Tool for JobStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read the current status of a background job.",
            json!({
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"],
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = required_str(&call.arguments, "job_id", Self::NAME)?;
        let job = self.jobs.get(job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            job_snapshot_json(job_id, &job, DEFAULT_TAIL_CHARS),
        ))
    }
}

#[derive(Debug, Clone)]
struct JobTailTool {
    jobs: JobStore,
}

impl JobTailTool {
    const NAME: &'static str = "job_tail";

    fn new(jobs: JobStore) -> Self {
        Self { jobs }
    }
}

impl Tool for JobTailTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Read bounded stdout/stderr tails for a background job.",
            json!({
                "type": "object",
                "properties": {
                    "job_id": {"type": "string"},
                    "max_chars": {"type": "integer", "description": "Tail size per stream, default 4000, max 20000"}
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = required_str(&call.arguments, "job_id", Self::NAME)?;
        let max_chars = optional_u64(
            &call.arguments,
            "max_chars",
            DEFAULT_TAIL_CHARS as u64,
            Self::NAME,
        )?
        .clamp(1, MAX_TAIL_CHARS as u64) as usize;
        let job = self.jobs.get(job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            job_snapshot_json(job_id, &job, max_chars),
        ))
    }
}

#[derive(Debug, Clone)]
struct JobCancelTool {
    jobs: JobStore,
}

impl JobCancelTool {
    const NAME: &'static str = "job_cancel";

    fn new(jobs: JobStore) -> Self {
        Self { jobs }
    }
}

impl Tool for JobCancelTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Cancel a running shell job. Requires approval because it changes process state.",
            json!({
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = required_str(&call.arguments, "job_id", Self::NAME)?;
        let job = self.jobs.get(job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        if job.status == JobStatus::Running {
            cancel_job(&mut job, Self::NAME)?;
        }
        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            job_snapshot_json(job_id, &job, DEFAULT_TAIL_CHARS),
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::tool::{ApprovalDecision, ToolResultStatus, ToolRunOutcome};

    fn registry(root: &std::path::Path) -> ToolRegistry {
        ShellTools::new(root)
            .unwrap()
            .with_sandbox(SandboxManager::new().force_sandbox(Some(false)))
            .into_registry()
    }

    fn approved(root: &std::path::Path, name: &str, arguments: Value) -> ToolResult {
        let call = ToolCall::new("call_1", name, arguments);
        let ToolRunOutcome::Result { result } = registry(root)
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        result
    }

    #[test]
    fn shell_run_requires_approval_and_returns_output() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new(
            "call_1",
            "shell_run",
            json!({"command": "python3 -c 'print(\"hello\")'"}),
        );

        assert!(matches!(
            registry.run_tool_call(call.clone(), None).unwrap(),
            ToolRunOutcome::ApprovalRequired { .. }
        ));
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(result.status, ToolResultStatus::Success);
        assert!(output["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(output["status"], "completed");
        assert_eq!(output["kind"], "foreground");
        let job_id = output["job_id"].as_str().unwrap();
        let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["kind"], "foreground");
    }

    #[test]
    fn shell_run_reports_failure() {
        let tmp = tempdir().unwrap();
        let result = approved(tmp.path(), "shell_run", json!({"command": "exit 7"}));
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "failed");
        assert_eq!(output["exit_code"], 7);
    }

    #[test]
    fn shell_run_times_out() {
        let tmp = tempdir().unwrap();
        let result = approved(
            tmp.path(),
            "shell_run",
            json!({"command": "sleep 1", "timeout_ms": 1}),
        );
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "timed_out");
    }

    #[test]
    fn long_shell_run_returns_cancellable_job_id() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new("call_1", "shell_run", json!({"command": "sleep 2"}));
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "running");
        let job_id = output["job_id"].as_str().unwrap();

        let cancel = ToolCall::new("call_2", "job_cancel", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(cancel, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "cancelled");
    }

    #[test]
    fn running_shell_run_preserves_timeout_on_job_status() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new(
            "call_1",
            "shell_run",
            json!({"command": "sleep 1", "timeout_ms": 150}),
        );
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "running");
        let job_id = output["job_id"].as_str().unwrap();

        thread::sleep(Duration::from_millis(220));
        let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "timed_out");
    }

    #[test]
    fn completed_shell_run_is_not_marked_timed_out_by_late_status_check() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new(
            "call_1",
            "shell_run",
            json!({"command": "sleep 0.2", "timeout_ms": 1000}),
        );
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "running");
        let job_id = output["job_id"].as_str().unwrap();

        thread::sleep(Duration::from_millis(1_100));
        let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "completed");
    }

    #[test]
    fn shell_run_times_out_without_polling_before_deadline() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new(
            "call_1",
            "shell_run",
            json!({"command": "sleep 1", "timeout_ms": 150}),
        );
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "running");
        let job_id = output["job_id"].as_str().unwrap();

        thread::sleep(Duration::from_millis(300));
        let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "timed_out");
    }

    #[test]
    fn shell_rejects_cwd_escape() {
        let tmp = tempdir().unwrap();
        let call = ToolCall::new(
            "call_1",
            "shell_run",
            json!({"command": "pwd", "cwd": "../outside"}),
        );
        assert!(matches!(
            registry(tmp.path()).run_tool_call(call, Some(ApprovalDecision::Approved)),
            Err(ToolError::InvalidArguments { .. })
        ));
    }

    #[test]
    fn job_start_status_tail_and_cancel_work() {
        let tmp = tempdir().unwrap();
        let registry = registry(tmp.path());
        let call = ToolCall::new(
            "call_1",
            "job_start",
            json!({"command": "printf hello && sleep 2"}),
        );
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        let job_id = output["job_id"].as_str().unwrap();

        thread::sleep(Duration::from_millis(50));
        let tail = ToolCall::new("call_2", "job_tail", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry.run_tool_call(tail, None).unwrap() else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["stdout_tail"], "hello");

        let cancel = ToolCall::new("call_3", "job_cancel", json!({"job_id": job_id}));
        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(cancel, Some(ApprovalDecision::Approved))
            .unwrap()
        else {
            panic!("expected result");
        };
        let output: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(output["status"], "cancelled");
    }
}
