mod jobs;

use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::sandbox::{SandboxManager, SandboxPolicy};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, run_blocking};
use crate::workspace_policy::{WorkspacePolicy, invalid, json_string};
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

    fn run_sync(
        &self,
        params: ShellRunParams,
        policy: &SandboxPolicy,
    ) -> Result<ToolOutput, ToolError> {
        let command = params.command.as_str();
        if command.trim().is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self.root.resolve_cwd(params.cwd.as_deref(), Self::NAME)?;
        let timeout_ms = params
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);

        let started = Instant::now();
        // Detach stdin from the console: an inherited child (e.g. `cmd /C` on
        // Windows) restores default console mode on exit, dropping our mouse
        // capture so the wheel turns into ↑/↓ keys in the TUI.
        let mut child = self
            .sandbox
            .wrap_shell_command(command, &cwd, self.root.root(), policy)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!("failed to start command: {error}"),
            })?;

        let job_guard = self.sandbox.confine_spawned(&child, policy);
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
            job_guard,
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
        Ok(ToolOutput::text(command_output_json(
            &job_id,
            &job,
            MAX_OUTPUT_CHARS,
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellRunParams {
    /// Shell command to execute
    command: String,
    /// Optional workspace-relative working directory
    cwd: Option<String>,
    /// Timeout in milliseconds, default 30000, max 300000
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for ShellRunTool {
    type Params = ShellRunParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run a foreground shell command inside the workspace with timeout, bounded output, and a cancellable job record. Requires approval because shell commands can modify the workspace."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: ShellRunParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        let policy = cx.sandbox_policy();
        run_blocking(Self::NAME, move || this.run_sync(params, &policy)).await
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

    fn start_sync(
        &self,
        params: JobStartParams,
        policy: &SandboxPolicy,
    ) -> Result<ToolOutput, ToolError> {
        let command = params.command.as_str();
        if command.trim().is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self.root.resolve_cwd(params.cwd.as_deref(), Self::NAME)?;
        let mut child = self
            .sandbox
            .wrap_shell_command(command, &cwd, self.root.root(), policy)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ToolError::ExecutionFailed {
                name: Self::NAME.to_string(),
                message: format!("failed to start background command: {error}"),
            })?;
        let job_guard = self.sandbox.confine_spawned(&child, policy);
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
            job_guard,
        });

        Ok(ToolOutput::text(json_string(json!({
            "job_id": job_id,
            "command": command,
            "cwd": self.root.relative_display(&cwd),
            "status": "running",
            "kind": "background",
            "approval_reason": "shell commands can modify files, run code, or access the network"
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobStartParams {
    command: String,
    /// Optional workspace-relative working directory
    cwd: Option<String>,
}

#[async_trait]
impl Tool for JobStartTool {
    type Params = JobStartParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Start a background shell command inside the workspace. Requires approval because shell commands can modify the workspace."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: JobStartParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        let policy = cx.sandbox_policy();
        run_blocking(Self::NAME, move || this.start_sync(params, &policy)).await
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

    fn status_sync(&self, params: JobStatusParams) -> Result<ToolOutput, ToolError> {
        let job = self.jobs.get(&params.job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        Ok(ToolOutput::text(job_snapshot_json(
            &params.job_id,
            &job,
            DEFAULT_TAIL_CHARS,
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobStatusParams {
    job_id: String,
}

#[async_trait]
impl Tool for JobStatusTool {
    type Params = JobStatusParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Read the current status of a background job."
    }

    async fn run(&self, params: JobStatusParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.status_sync(params)).await
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

    fn tail_sync(&self, params: JobTailParams) -> Result<ToolOutput, ToolError> {
        let max_chars = params
            .max_chars
            .unwrap_or(DEFAULT_TAIL_CHARS as u64)
            .clamp(1, MAX_TAIL_CHARS as u64) as usize;
        let job = self.jobs.get(&params.job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        Ok(ToolOutput::text(job_snapshot_json(
            &params.job_id,
            &job,
            max_chars,
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobTailParams {
    job_id: String,
    /// Tail size per stream, default 4000, max 20000
    max_chars: Option<u64>,
}

#[async_trait]
impl Tool for JobTailTool {
    type Params = JobTailParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Read bounded stdout/stderr tails for a background job."
    }

    async fn run(&self, params: JobTailParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.tail_sync(params)).await
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

    fn cancel_sync(&self, params: JobCancelParams) -> Result<ToolOutput, ToolError> {
        let job = self.jobs.get(&params.job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        if job.status == JobStatus::Running {
            cancel_job(&mut job, Self::NAME)?;
        }
        Ok(ToolOutput::text(job_snapshot_json(
            &params.job_id,
            &job,
            DEFAULT_TAIL_CHARS,
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobCancelParams {
    job_id: String,
}

#[async_trait]
impl Tool for JobCancelTool {
    type Params = JobCancelParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Cancel a running shell job. Requires approval because it changes process state."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: JobCancelParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.cancel_sync(params)).await
    }
}

#[cfg(test)]
#[path = "shell_tools/tests.rs"]
mod tests;
