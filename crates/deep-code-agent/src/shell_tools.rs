mod jobs;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::sandbox::SandboxManager;
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, ToolUpdate};
use crate::workspace_policy::{WorkspacePolicy, invalid};
#[allow(unused_imports)]
pub use jobs::{BackgroundJobSummary, JobStore};
use jobs::{
    ChunkFn, JobKind, JobState, JobStatus, SharedBuffer, cancel_job, job_details,
    job_text_snapshot, refresh_job, shell_text_output, spawn_buffer_reader,
};

/// Strip provider/runtime secrets from a tool subprocess before it is spawned.
///
/// These live in the parent process environment because the LLM client reads
/// the API key at startup and the HTTP server reads the auth token on bind —
/// but no shell/job tool ever needs them. Removing them keeps an injected
/// command from lifting the key straight out of its own environment
/// (`echo $DEEPSEEK_API_KEY`, `curl -d "$DEEPSEEK_API_KEY"`).
///
/// This narrows exposure; it does not fully close it. A same-uid child can
/// still read the parent's `/proc/<ppid>/environ` (reads are not sandboxed),
/// which is out of scope for this hardening.
fn scrub_secret_env(cmd: &mut tokio::process::Command) {
    for var in crate::config::SUBPROCESS_SECRET_ENV {
        cmd.env_remove(var);
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
const MAX_OUTPUT_CHARS: usize = 20_000;
const DEFAULT_TAIL_CHARS: u64 = 4_000;
const MAX_TAIL_CHARS: u64 = 20_000;
/// Cap on bytes streamed live through `cx.update` per shell call (matches the
/// ring size; the final output still carries the tail beyond this).
const MAX_STREAMED_BYTES: usize = 128 * 1024;

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

    #[cfg(test)]
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
        registry.register(ShellTool::new(
            self.root.clone(),
            self.jobs.clone(),
            self.sandbox.clone(),
        ));
        registry.register(JobTool::new(self.root, self.jobs, self.sandbox));
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

/// Foreground shell: streams output live via `cx.update`, kills the child at
/// the deadline, and records the run in the job store so `GET /jobs` and
/// `job action=tail` can see it afterwards.
#[derive(Debug, Clone)]
struct ShellTool {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
}

impl ShellTool {
    const NAME: &'static str = "shell";

    fn new(root: WorkspacePolicy, jobs: JobStore, sandbox: SandboxManager) -> Self {
        Self {
            root,
            jobs,
            sandbox,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellParams {
    /// Shell command to execute
    command: String,
    /// Optional workspace-relative working directory
    cwd: Option<String>,
    /// Timeout in seconds, default 30, max 300; the command is killed at the deadline
    timeout_secs: Option<u64>,
}

/// Live-stream one output chunk as a ToolUpdate, bounded by a shared budget.
fn stream_chunk_fn(cx: &ToolCx, stream: &'static str, budget: Arc<AtomicUsize>) -> ChunkFn {
    let cx = cx.clone();
    Arc::new(move |bytes: &[u8]| {
        let used = budget.fetch_add(bytes.len(), Ordering::Relaxed);
        if used >= MAX_STREAMED_BYTES {
            return;
        }
        cx.update(ToolUpdate {
            text: String::from_utf8_lossy(bytes).to_string(),
            details: Some(json!({ "stream": stream })),
        });
    })
}

#[async_trait]
impl Tool for ShellTool {
    type Params = ShellParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run a foreground shell command in the workspace; output streams live and the process is killed at the timeout. Use it for git (status/diff/log), builds, and tests; start long-running processes (dev servers) with the job tool instead."
    }

    async fn run(&self, params: ShellParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let command = params.command.trim().to_string();
        if command.is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self.root.resolve_cwd(params.cwd.as_deref(), Self::NAME)?;
        let timeout = Duration::from_secs(
            params
                .timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );
        let policy = cx.sandbox_policy();

        let started = Instant::now();
        // Detach stdin from the console: an inherited child (e.g. `cmd /C` on
        // Windows) restores default console mode on exit, dropping our mouse
        // capture so the wheel turns into ↑/↓ keys in the TUI.
        let std_cmd = self
            .sandbox
            .wrap_shell_command(&command, &cwd, self.root.root(), &policy);
        let mut cmd = tokio::process::Command::from(std_cmd);
        scrub_secret_env(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to start command: {error}"),
        })?;

        let job_guard = self.sandbox.confine_spawned(&child, &policy);
        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        let stream_budget = Arc::new(AtomicUsize::new(0));
        if let Some(pipe) = child.stdout.take() {
            spawn_buffer_reader(
                pipe,
                stdout.clone(),
                Some(stream_chunk_fn(cx, "stdout", Arc::clone(&stream_budget))),
            );
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_buffer_reader(
                pipe,
                stderr.clone(),
                Some(stream_chunk_fn(cx, "stderr", stream_budget)),
            );
        }

        // The tool future owns the child; the store entry exposes the run to
        // `GET /jobs` and post-hoc `job action=tail`.
        let job_id = self.jobs.insert(JobState {
            kind: JobKind::Foreground,
            command: command.clone(),
            cwd: self.root.relative_display(&cwd),
            started_at: started,
            status: JobStatus::Running,
            exit_code: None,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            child: None,
            job_guard,
        });

        let (status, exit_code) = tokio::select! {
            result = child.wait() => match result {
                Ok(exit) => (
                    if exit.success() { JobStatus::Completed } else { JobStatus::Failed },
                    exit.code(),
                ),
                Err(error) => {
                    return Err(ToolError::ExecutionFailed {
                        name: Self::NAME.to_string(),
                        message: format!("failed to wait for command: {error}"),
                    });
                }
            },
            () = cx.cancel_token().cancelled() => {
                let _ = child.kill().await;
                (JobStatus::Cancelled, None)
            }
            () = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                (JobStatus::TimedOut, None)
            }
        };

        // Give the reader tasks a beat to drain the final pipe chunks.
        tokio::task::yield_now().await;

        let job = self.jobs.get(&job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        job.status = status;
        job.exit_code = exit_code;
        let content = shell_text_output(&job_id, &job, MAX_OUTPUT_CHARS);
        let details = job_details(&job_id, &job);
        Ok(ToolOutput::text(content).with_details(details))
    }
}

/// Background job management: `action=start` launches, `status`/`tail`
/// inspect, `cancel` kills.
#[derive(Debug, Clone)]
struct JobTool {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
}

impl JobTool {
    const NAME: &'static str = "job";

    fn new(root: WorkspacePolicy, jobs: JobStore, sandbox: SandboxManager) -> Self {
        Self {
            root,
            jobs,
            sandbox,
        }
    }

    async fn start(&self, params: &JobParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let command = params
            .command
            .as_deref()
            .ok_or_else(|| invalid(Self::NAME, "action=start requires 'command'"))?
            .trim()
            .to_string();
        if command.is_empty() {
            return Err(invalid(Self::NAME, "command must not be empty"));
        }
        let cwd = self.root.resolve_cwd(params.cwd.as_deref(), Self::NAME)?;
        let policy = cx.sandbox_policy();

        let std_cmd = self
            .sandbox
            .wrap_shell_command(&command, &cwd, self.root.root(), &policy);
        let mut cmd = tokio::process::Command::from(std_cmd);
        scrub_secret_env(&mut cmd);
        // Tie the process lifetime to its stored `Child`: if the JobStore is
        // dropped (app exit) the OS process is killed rather than orphaned.
        // `JobStore::shutdown` makes this deterministic on cancel/quit.
        cmd.kill_on_drop(true);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|error| ToolError::ExecutionFailed {
            name: Self::NAME.to_string(),
            message: format!("failed to start background command: {error}"),
        })?;
        let job_guard = self.sandbox.confine_spawned(&child, &policy);
        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        if let Some(pipe) = child.stdout.take() {
            spawn_buffer_reader(pipe, stdout.clone(), None);
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_buffer_reader(pipe, stderr.clone(), None);
        }

        let job_id = self.jobs.insert(JobState {
            kind: JobKind::Background,
            command: command.clone(),
            cwd: self.root.relative_display(&cwd),
            started_at: Instant::now(),
            status: JobStatus::Running,
            exit_code: None,
            stdout,
            stderr,
            child: Some(child),
            job_guard,
        });

        let job = self.jobs.get(&job_id, Self::NAME)?;
        let details = {
            let job = job.lock().expect("job lock poisoned");
            job_details(&job_id, &job)
        };
        Ok(
            ToolOutput::text(format!("started {job_id} (background): {command}"))
                .with_details(details),
        )
    }

    fn snapshot(&self, params: &JobParams, max_chars: usize) -> Result<ToolOutput, ToolError> {
        let job_id = require_job_id(params)?;
        let job = self.jobs.get(job_id, Self::NAME)?;
        let mut job = job.lock().expect("job lock poisoned");
        refresh_job(&mut job);
        Ok(ToolOutput::text(job_text_snapshot(job_id, &job, max_chars))
            .with_details(job_details(job_id, &job)))
    }

    async fn cancel(&self, params: &JobParams) -> Result<ToolOutput, ToolError> {
        let job_id = require_job_id(params)?;
        let job = self.jobs.get(job_id, Self::NAME)?;
        {
            let mut guard = job.lock().expect("job lock poisoned");
            refresh_job(&mut guard);
        }
        cancel_job(&job, Self::NAME).await?;
        let job = job.lock().expect("job lock poisoned");
        Ok(
            ToolOutput::text(job_text_snapshot(job_id, &job, DEFAULT_TAIL_CHARS as usize))
                .with_details(job_details(job_id, &job)),
        )
    }
}

fn require_job_id(params: &JobParams) -> Result<&str, ToolError> {
    params
        .job_id
        .as_deref()
        .ok_or_else(|| invalid(JobTool::NAME, "this action requires 'job_id'"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum JobAction {
    Start,
    Status,
    Tail,
    Cancel,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobParams {
    /// start launches a background command, status/tail inspect a job, cancel kills it
    action: JobAction,
    /// Shell command to launch (required for action=start)
    command: Option<String>,
    /// Optional workspace-relative working directory (start only)
    cwd: Option<String>,
    /// Job id from a previous start (required for status/tail/cancel)
    job_id: Option<String>,
    /// Tail size per stream for action=tail, default 4000, max 20000
    max_chars: Option<u64>,
}

#[async_trait]
impl Tool for JobTool {
    type Params = JobParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Manage background shell jobs: action=start launches a command in the background, status/tail inspect it, cancel kills it."
    }

    async fn run(&self, params: JobParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        match params.action {
            JobAction::Start => self.start(&params, cx).await,
            JobAction::Status => self.snapshot(&params, DEFAULT_TAIL_CHARS as usize),
            JobAction::Tail => {
                let max_chars = params
                    .max_chars
                    .unwrap_or(DEFAULT_TAIL_CHARS)
                    .clamp(1, MAX_TAIL_CHARS) as usize;
                self.snapshot(&params, max_chars)
            }
            JobAction::Cancel => self.cancel(&params).await,
        }
    }
}

#[cfg(test)]
#[path = "shell_tools/tests.rs"]
mod tests;
