use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{BufReader, Read};
use std::process::{Child, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::sandbox::SandboxManager;
use crate::tool::{Tool, ToolCall, ToolError, ToolRegistry, ToolResult, ToolSpec};
use crate::tool_execution::current_sandbox_policy;
use crate::workspace_policy::{
    WorkspacePolicy, invalid, json_string, optional_str, optional_u64, required_str,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_OUTPUT_CHARS: usize = 20_000;
const DEFAULT_TAIL_CHARS: usize = 4_000;
const MAX_TAIL_CHARS: usize = 20_000;
const JOB_BUFFER_BYTES: usize = 128 * 1024;
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

#[derive(Debug, Clone, Default)]
pub struct JobStore {
    next_id: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobState>>>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJobSummary {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub background: bool,
}

impl JobStore {
    /// Summaries of shell jobs tracked for the current runtime (foreground + background).
    pub fn list_summaries(&self) -> Vec<BackgroundJobSummary> {
        let guard = self.jobs.lock().expect("job store lock poisoned");
        let mut summaries: Vec<_> = guard
            .iter()
            .filter_map(|(id, state_arc)| {
                let state = state_arc.lock().ok()?;
                Some(BackgroundJobSummary {
                    id: id.clone(),
                    command: state.command.clone(),
                    cwd: state.cwd.clone(),
                    status: state.status,
                    exit_code: state.exit_code,
                    background: state.kind == JobKind::Background,
                })
            })
            .collect();
        summaries.sort_by(|left, right| right.id.cmp(&left.id));
        summaries
    }

    fn insert(&self, state: JobState) -> String {
        let id = format!("job_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        self.jobs
            .lock()
            .expect("job store lock poisoned")
            .insert(id.clone(), Arc::new(Mutex::new(state)));
        id
    }

    fn get(&self, id: &str, tool_name: &str) -> Result<Arc<Mutex<JobState>>, ToolError> {
        self.jobs
            .lock()
            .expect("job store lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(tool_name, format!("unknown job_id '{id}'")))
    }
}

#[derive(Debug)]
struct JobState {
    kind: JobKind,
    command: String,
    cwd: String,
    started_at: Instant,
    timeout_deadline: Option<Instant>,
    status: JobStatus,
    exit_code: Option<i32>,
    stdout: SharedBuffer,
    stderr: SharedBuffer,
    child: Option<Child>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone)]
struct SharedBuffer(Arc<Mutex<RingBuffer>>);

impl Default for SharedBuffer {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(RingBuffer::new(JOB_BUFFER_BYTES))))
    }
}

impl SharedBuffer {
    fn push(&self, bytes: &[u8]) {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .push(bytes);
    }

    fn text(&self) -> String {
        self.0.lock().expect("output buffer lock poisoned").text()
    }

    fn total_len(&self) -> usize {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .total_len
    }

    fn omitted_len(&self) -> usize {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .omitted_len()
    }
}

#[derive(Debug)]
struct RingBuffer {
    bytes: VecDeque<u8>,
    capacity: usize,
    total_len: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            capacity,
            total_len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_len += bytes.len();
        for byte in bytes {
            if self.bytes.len() == self.capacity {
                self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>()).to_string()
    }

    fn omitted_len(&self) -> usize {
        self.total_len.saturating_sub(self.bytes.len())
    }
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
            command_output_json(&job_id, &job),
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

fn spawn_buffer_reader<R: Read + Send + 'static>(pipe: R, buffer: SharedBuffer) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut chunk = [0_u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(bytes) => buffer.push(&chunk[..bytes]),
                Err(_) => break,
            }
        }
    });
}

fn refresh_job(job: &mut JobState) {
    if job.status != JobStatus::Running {
        return;
    }
    let Some(child) = job.child.as_mut() else {
        return;
    };
    if let Ok(Some(status)) = child.try_wait() {
        job.exit_code = status.code();
        job.status = if status.success() {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        job.child = None;
        return;
    }
    if job
        .timeout_deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        timeout_job(job);
    }
}

fn timeout_job(job: &mut JobState) {
    if let Some(child) = job.child.as_mut() {
        let _ = child.kill();
        if let Ok(status) = child.wait() {
            job.exit_code = status.code();
        }
    }
    job.child = None;
    job.status = JobStatus::TimedOut;
}

fn cancel_job(job: &mut JobState, tool_name: &str) -> Result<(), ToolError> {
    if let Some(child) = job.child.as_mut() {
        let _ = child.kill();
        let status = child.wait().map_err(|error| ToolError::ExecutionFailed {
            name: tool_name.to_string(),
            message: format!("failed to wait after cancel: {error}"),
        })?;
        job.exit_code = status.code();
    }
    job.child = None;
    job.status = JobStatus::Cancelled;
    Ok(())
}

fn command_output_json(job_id: &str, job: &JobState) -> String {
    let stdout = job.stdout.text();
    let stderr = job.stderr.text();
    let stdout_len = job.stdout.total_len();
    let stderr_len = job.stderr.total_len();
    let stdout_omitted_chars = job.stdout.omitted_len();
    let stderr_omitted_chars = job.stderr.omitted_len();
    let stdout = tail_chars(&stdout, MAX_OUTPUT_CHARS);
    let stderr = tail_chars(&stderr, MAX_OUTPUT_CHARS);
    json_string(json!({
        "job_id": job_id,
        "kind": job.kind,
        "command": job.command,
        "cwd": job.cwd,
        "status": job.status,
        "exit_code": job.exit_code,
        "elapsed_ms": job.started_at.elapsed().as_millis() as u64,
        "stdout": stdout,
        "stderr": stderr,
        "stdout_len": stdout_len,
        "stderr_len": stderr_len,
        "stdout_truncated": stdout_omitted_chars > 0,
        "stderr_truncated": stderr_omitted_chars > 0,
        "stdout_omitted_chars": stdout_omitted_chars,
        "stderr_omitted_chars": stderr_omitted_chars,
        "approval_reason": "shell commands can modify files, run code, or access the network"
    }))
}

fn job_snapshot_json(job_id: &str, job: &JobState, max_chars: usize) -> String {
    let stdout = job.stdout.text();
    let stderr = job.stderr.text();
    let stdout_len = job.stdout.total_len();
    let stderr_len = job.stderr.total_len();
    let stdout_tail = tail_chars(&stdout, max_chars);
    let stderr_tail = tail_chars(&stderr, max_chars);
    json_string(json!({
        "job_id": job_id,
        "kind": job.kind,
        "command": job.command,
        "cwd": job.cwd,
        "status": job.status,
        "exit_code": job.exit_code,
        "elapsed_ms": job.started_at.elapsed().as_millis() as u64,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
        "stdout_len": stdout_len,
        "stderr_len": stderr_len,
        "stdout_omitted_chars": job.stdout.omitted_len(),
        "stderr_omitted_chars": job.stderr.omitted_len()
    }))
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    value.chars().skip(count - max_chars).collect()
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

    #[test]
    fn job_output_buffer_is_bounded_with_omitted_count() {
        let buffer = SharedBuffer::default();
        buffer.push(&vec![b'a'; JOB_BUFFER_BYTES + 10]);

        assert_eq!(buffer.total_len(), JOB_BUFFER_BYTES + 10);
        assert_eq!(buffer.omitted_len(), 10);
        assert_eq!(buffer.text().len(), JOB_BUFFER_BYTES);
    }
}
