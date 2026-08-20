mod jobs;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use std::path::{Path, PathBuf};

use crate::sandbox::{Enforcement, SandboxGuard, SandboxManager, SandboxPolicy};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, ToolUpdate};
#[cfg(test)]
use crate::workspace_policy::WorkspaceRoots;
use crate::workspace_policy::{WorkspacePolicy, invalid};
#[allow(unused_imports)]
pub use jobs::JobStore;
use jobs::{
    ChunkFn, JobKind, JobState, JobStatus, SharedBuffer, cancel_job, job_details,
    job_text_snapshot, kill_process_tree, refresh_job, shell_text_output, spawn_buffer_reader,
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

/// Teaches the model to mine the spill file instead of re-running commands
/// with grep/head filters just to see a different slice of the output.
/// Appended to all four shell/job descriptions — spilling is not a sandbox
/// concern, so confined and unconfined hosts behave identically.
const SPILL_DESC: &str = " When output overflows the inline window, the complete stream is saved to a file under .deep-code/spill/ and the result names its absolute path — grep or read that file instead of re-running the command with filters.";

/// Shell tool description for hosts whose sandbox really confines the command.
const SHELL_DESC_CONFINED: &str = "Run a foreground shell command in the workspace; output streams live and the process is killed at the timeout. Use it for git (status/diff/log), builds, and tests; start long-running processes (dev servers) with the job tool instead. Commands run sandboxed without network; set network=true when one needs it (installs, git remote ops) — a failed download/connection usually means the run lacked the network grant. Writes are confined to the granted roots (the workspace and --add-dir directories): a write outside them fails with e.g. 'Operation not permitted', and no retry, sudo or chmod can succeed — request the directory with the request_write_root tool instead (the user decides).";

/// Same, for hosts with no real confinement (see `SandboxCapabilities`). The
/// model is told the truth so it neither trusts a boundary that is absent nor
/// assumes it is offline when it is not.
const SHELL_DESC_UNCONFINED: &str = "Run a foreground shell command in the workspace; output streams live and the process is killed at the timeout. Use it for git (status/diff/log), builds, and tests; start long-running processes (dev servers) with the job tool instead. This host has NO OS sandbox confinement: commands are not restricted to the workspace and do have network access. Keep writes inside the workspace yourself and avoid destructive commands. Still set network=true when a command needs the network, so the user is asked first.";

/// Job tool description, confined host.
const JOB_DESC_CONFINED: &str = "Manage background shell jobs: action=start launches a command in the background, status/tail inspect it, cancel kills it. Jobs run sandboxed without network; a dev server that binds a port needs network=true on start. Writes are confined to the granted roots (workspace and --add-dir directories); a denied write cannot be fixed by retrying — request the directory with the request_write_root tool instead (the user decides).";

/// Job tool description, unconfined host.
const JOB_DESC_UNCONFINED: &str = "Manage background shell jobs: action=start launches a command in the background, status/tail inspect it, cancel kills it. This host has NO OS sandbox confinement: jobs are not restricted to the workspace and do have network access. Still set network=true when starting something that binds a port or needs the network, so the user is asked first.";

/// Builds the description matching what this host actually enforces.
///
/// A partial host gets the confined body plus one sentence *per gap*, taken from
/// [`crate::sandbox::EnforcementGap::model_caveat`], rather than a fixed "kernel is
/// too old" paragraph. The fixed paragraph was wrong on most Linux machines: it
/// denied the write boundary outright, while the only gap below Linux 6.10 is
/// the device-`ioctl` one, which leaves that boundary fully intact. The model is
/// the surface that actually issues the write, so it is the last place a gap may
/// be rounded — in either direction.
///
/// Any confined host (Full or Partial) additionally gets `notes` — refusals the
/// sandbox imposes *by design* ([`crate::sandbox::sandbox_design_notes`]). A
/// deliberate denial the model will run into must be disclosed for the same
/// reason a gap must: its failure text ("Permission denied") reads exactly like
/// a write-boundary denial, and a model that cannot tell them apart chases
/// `/add-dir` over a failure no grant can fix.
fn describe(
    confined: &'static str,
    unconfined: &'static str,
    enforcement: &Enforcement,
    notes: &[&str],
) -> String {
    if !enforcement.is_enforced() {
        return format!("{unconfined}{SPILL_DESC}");
    }
    // Spill is tool behavior, not a sandbox property: it joins the body on
    // both branches, BEFORE the enforcement caveats — design notes keep the
    // last word about what the sandbox refuses.
    let mut text = format!("{confined}{SPILL_DESC}");
    for gap in enforcement.gaps() {
        text.push(' ');
        text.push_str(gap.model_caveat());
    }
    for note in notes {
        text.push(' ');
        text.push_str(note);
    }
    text
}

/// Memoized: `description()` is called for every tool-registry build (each
/// subagent gets one) and the answer cannot change under a running process.
fn shell_description() -> &'static str {
    static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DESC.get_or_init(|| {
        describe(
            SHELL_DESC_CONFINED,
            SHELL_DESC_UNCONFINED,
            crate::sandbox::sandbox_enforcement(),
            crate::sandbox::sandbox_design_notes(),
        )
    })
}

fn job_description() -> &'static str {
    static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DESC.get_or_init(|| {
        describe(
            JOB_DESC_CONFINED,
            JOB_DESC_UNCONFINED,
            crate::sandbox::sandbox_enforcement(),
            crate::sandbox::sandbox_design_notes(),
        )
    })
}

/// Build, secret-scrub, sandbox-wrap and spawn one shell subprocess, then
/// confine it. Shared by the foreground and background job paths so both apply
/// the identical env-scrub + sandbox treatment; they differ only afterwards in
/// how they read output and whether they retain the child handle.
fn spawn_confined(
    sandbox: &SandboxManager,
    granted_roots: &[PathBuf],
    command: &str,
    cwd: &Path,
    policy: &SandboxPolicy,
    tool_name: &str,
    error_context: &str,
) -> Result<(tokio::process::Child, Option<SandboxGuard>), ToolError> {
    // Refuse rather than run bare when the policy wanted a sandbox but this
    // host has no backend: the safety model treats the OS sandbox as the real
    // boundary, so a command that would otherwise escape unconfined must not
    // silently run (mirrors the eval harness's own refuse-if-unenforceable guard).
    if sandbox.sandbox_unavailable_for(policy) {
        // Include the probe's own diagnosis and a next step. Without them this
        // read as "this platform is unsupported" with nothing to act on — the
        // detail says *why* (e.g. "Landlock unavailable: ..."), and `doctor`
        // prints the full capability report.
        let detail = crate::sandbox::detect_capabilities().detail;
        return Err(ToolError::exec_failed(
            tool_name,
            format!(
                "{error_context}: refusing to run without an OS sandbox — the command \
                 would run with unconfined host access. Cause: {detail}. Run \
                 `doctor` for the full sandbox report."
            ),
        ));
    }
    // `mut` is only exercised by the Unix process-group call below; on other
    // platforms the binding is moved as-is into `Command::from`.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut std_cmd = sandbox
        .wrap_shell_command(command, cwd, granted_roots, policy)
        .map_err(|detail| {
            ToolError::exec_failed(
                tool_name,
                format!(
                    "{error_context}: refusing to run — the OS sandbox could not be \
                     applied to this command: {detail}"
                ),
            )
        })?;
    // Own process group (Unix) so timeout/cancel/shutdown can kill the whole
    // tree via `kill_process_tree`, not just the immediate shell.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        std_cmd.process_group(0);
    }
    let mut cmd = tokio::process::Command::from(std_cmd);
    scrub_secret_env(&mut cmd);
    // Detach stdin from the console (an inherited child restoring console mode
    // on exit would drop our mouse capture), pipe both output streams, and kill
    // the process if the handle is dropped.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|error| ToolError::exec_failed(tool_name, format!("{error_context}: {error}")))?;
    let guard = sandbox.confine_spawned(&child, policy);
    Ok((child, guard))
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
    spill_dir: PathBuf,
}

/// The workspace's spill home: `<primary>/.deep-code/spill`.
fn spill_home(primary_root: &Path) -> PathBuf {
    primary_root.join(".deep-code").join("spill")
}

/// Per-instance spill directory under the primary root's `.deep-code`.
///
/// Inside the workspace on purpose: that keeps every read path open with zero
/// policy changes — `read_file`/`grep_files` resolve it as granted, sandboxed
/// shell commands can read it on all three platforms (the Seatbelt read-deny
/// covers only the HOME `~/.deep-code` secret store), checkpoints and the
/// default grep walk already skip `.deep-code`. The launch component makes
/// names collision-free across processes AND across the per-subagent tool
/// registries within one process (each builds its own `ShellTools`, so job
/// ids alone would collide). Created lazily by the first spill — a session
/// that never overflows leaves no directory behind.
fn new_spill_dir(primary_root: &Path) -> PathBuf {
    static LAUNCH_SEQ: AtomicUsize = AtomicUsize::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    spill_home(primary_root).join(format!(
        "run-{millis}-{}-{}",
        std::process::id(),
        LAUNCH_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// How long a spill run outlives its session. Spill files must survive the
/// job record and the process — a transcript names their paths and a resumed
/// session may still mine last week's build log — but not forever: a single
/// overflowing job can leave up to 128 MB behind (64 MB per stream), and
/// nothing else ever deletes it. Checkpoints prune by count; spill prunes by
/// age, because a reference from an old transcript loses value with time
/// while a fresh one must keep working.
const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Best-effort removal of spill runs whose directory mtime — and every file
/// inside — predates `cutoff`. Two clocks on purpose: a NEW spill file moves
/// the directory mtime, but APPENDS to an existing file move only that
/// file's own, so a week-old run whose job is still streaming survives via
/// the newest-file check. Only `run-*` directories are touched (symlinked
/// entries are skipped — their file type reads as symlink, not dir), the
/// spill home itself is never created here, and every failure is ignored:
/// retention is disk hygiene, not correctness.
fn prune_stale_spill_runs(spill_home: &Path, cutoff: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(spill_home) else {
        return;
    };
    for entry in entries.flatten() {
        let is_run = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("run-"));
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_run || !is_dir {
            continue;
        }
        let dir_stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|mtime| mtime < cutoff);
        let files_stale = || newest_file_mtime(&entry.path()).is_none_or(|newest| newest < cutoff);
        if dir_stale && files_stale() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Newest modification time among a run's files; `None` for an unreadable or
/// empty run (the directory's own mtime already voted stale by then).
fn newest_file_mtime(run: &Path) -> Option<std::time::SystemTime> {
    std::fs::read_dir(run)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
}

impl ShellTools {
    /// Test convenience: own-policy construction. Production launches build
    /// ONE shared policy and use [`Self::with_policy`].
    #[cfg(test)]
    pub fn new(roots: impl Into<WorkspaceRoots>) -> Result<Self, ToolError> {
        Ok(Self::with_policy(WorkspacePolicy::new(roots)?))
    }

    /// Build on an existing (shared) boundary policy instead of constructing
    /// one. This is how a launch threads ONE policy through every tool group,
    /// so a mid-session `request_write_root` grant reaches shell commands and
    /// file tools alike without rebuilding any registry.
    pub(crate) fn with_policy(root: WorkspacePolicy) -> Self {
        // Construction is the retention hook: it runs at every launch (and
        // sub-agent spawn), detached — removing a stale spill tree can be
        // hundreds of MB of I/O, which must not stall the launch. Racing
        // this instance's own run dir below is harmless: a freshly created
        // dir can never test stale against a week-old cutoff.
        if let Some(cutoff) = std::time::SystemTime::now().checked_sub(SPILL_RETENTION) {
            let home = spill_home(root.root());
            std::thread::spawn(move || prune_stale_spill_runs(&home, cutoff));
        }
        let spill_dir = new_spill_dir(root.root());
        Self {
            root,
            jobs: JobStore::default(),
            sandbox: SandboxManager::new(),
            spill_dir,
        }
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
            self.spill_dir.clone(),
        ));
        registry.register(JobTool::new(
            self.root,
            self.jobs,
            self.sandbox,
            self.spill_dir,
        ));
        registry
    }
}

/// Test convenience wrapper over [`shell_tool_registry_from`].
#[cfg(test)]
pub fn shell_tool_registry(
    roots: impl Into<WorkspaceRoots>,
) -> Result<(ToolRegistry, JobStore), ToolError> {
    Ok(shell_tool_registry_from(WorkspacePolicy::new(roots)?))
}

/// Registry from a shared boundary policy (see [`ShellTools::with_policy`]).
pub(crate) fn shell_tool_registry_from(policy: WorkspacePolicy) -> (ToolRegistry, JobStore) {
    let shell = ShellTools::with_policy(policy);
    let jobs = shell.job_store();
    (shell.into_registry(), jobs)
}

/// Foreground shell: streams output live via `cx.update`, kills the child at
/// the deadline, and records the run in the job store so `GET /jobs` and
/// `job action=tail` can see it afterwards.
#[derive(Debug, Clone)]
struct ShellTool {
    root: WorkspacePolicy,
    jobs: JobStore,
    sandbox: SandboxManager,
    spill_dir: PathBuf,
}

impl ShellTool {
    const NAME: &'static str = "shell";

    fn new(
        root: WorkspacePolicy,
        jobs: JobStore,
        sandbox: SandboxManager,
        spill_dir: PathBuf,
    ) -> Self {
        Self {
            root,
            jobs,
            sandbox,
            spill_dir,
        }
    }
}

/// One spill-backed buffer pair for a reserved job id.
fn spill_buffers(spill_dir: &Path, job_id: &str) -> (SharedBuffer, SharedBuffer) {
    (
        SharedBuffer::with_spill(spill_dir.join(format!("{job_id}.stdout.log"))),
        SharedBuffer::with_spill(spill_dir.join(format!("{job_id}.stderr.log"))),
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellParams {
    /// Shell command to execute
    command: String,
    /// Optional workspace-relative working directory (absolute allowed only inside a granted root)
    cwd: Option<String>,
    /// Timeout in seconds, default 30, max 300; the command is killed at the deadline
    timeout_secs: Option<u64>,
    /// Set true when the command needs network access (downloads/installs, git push/pull/fetch/clone, curl). The sandbox blocks all network by default; a declaration routes through user approval.
    #[allow(dead_code)] // consumed by the execution policy from the raw arguments
    network: Option<bool>,
    /// One short sentence for the human at the approval prompt: why this command needs what it asks for (most useful with network=true). Shown as your claim, verbatim.
    #[allow(dead_code)] // surfaced to the approval prompt from the raw arguments
    justification: Option<String>,
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
        // The confinement sentence must match reality per host. Telling the model
        // "sandboxed without network" where nothing enforces it (the Windows Job
        // Object confines neither writes nor egress) teaches it a false model of
        // its own environment: it would skip declaring network it silently
        // already has, and assume out-of-workspace writes get refused for it.
        //
        // Keyed on `sandbox_enforcement` — the weaker of both dimensions —
        // and not on the network one alone: a description that promises write
        // confinement must be chosen by what confines writes. The two can now
        // differ in level, and the model is the thing that actually issues the
        // write, so it is the last surface that may round a gap away.
        shell_description()
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
        let (mut child, job_guard) = spawn_confined(
            &self.sandbox,
            &self.root.granted_roots(),
            &command,
            &cwd,
            &policy,
            Self::NAME,
            "failed to start command",
        )?;
        let job_id = self.jobs.reserve_id();
        let (stdout, stderr) = spill_buffers(&self.spill_dir, &job_id);
        let stream_budget = Arc::new(AtomicUsize::new(0));
        let stdout_task = child.stdout.take().map(|pipe| {
            spawn_buffer_reader(
                pipe,
                stdout.clone(),
                Some(stream_chunk_fn(cx, "stdout", Arc::clone(&stream_budget))),
            )
        });
        let stderr_task = child.stderr.take().map(|pipe| {
            spawn_buffer_reader(
                pipe,
                stderr.clone(),
                Some(stream_chunk_fn(cx, "stderr", stream_budget)),
            )
        });

        // The tool future owns the child; the store entry exposes the run to
        // post-hoc `job action=status/tail`.
        self.jobs.insert_with_id(
            &job_id,
            JobState {
                kind: JobKind::Foreground,
                command: command.clone(),
                cwd: self.root.relative_display(&cwd),
                started_at: started,
                status: JobStatus::Running,
                exit_code: None,
                sandboxed: self.sandbox.should_sandbox(&policy),
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                child: None,
                job_guard,
            },
        );

        let (status, exit_code) = tokio::select! {
            result = child.wait() => match result {
                Ok(exit) => (
                    if exit.success() { JobStatus::Completed } else { JobStatus::Failed },
                    exit.code(),
                ),
                Err(error) => {
                    return Err(ToolError::exec_failed(
                        Self::NAME,
                        format!("failed to wait for command: {error}"),
                    ));
                }
            },
            () = cx.cancel_token().cancelled() => {
                kill_process_tree(&mut child);
                let _ = child.wait().await;
                (JobStatus::Cancelled, None)
            }
            () = tokio::time::sleep(timeout) => {
                kill_process_tree(&mut child);
                let _ = child.wait().await;
                (JobStatus::TimedOut, None)
            }
        };

        // Await the reader tasks so the final pipe chunks land in the buffers
        // (a bare yield loses them under scheduler load). EOF is prompt once
        // the child is gone; the cap guards a lingering grandchild that
        // inherited the pipe and keeps it open past the parent's exit.
        let stdout_abort = stdout_task.as_ref().map(|task| task.abort_handle());
        let stderr_abort = stderr_task.as_ref().map(|task| task.abort_handle());
        let drain = async {
            if let Some(task) = stdout_task {
                let _ = task.await;
            }
            if let Some(task) = stderr_task {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(500), drain)
            .await
            .is_err()
        {
            // A grandchild that inherited the pipe kept it open past the cap:
            // abort the reader tasks so they (and the pipe fds they hold) don't
            // linger until that process finally exits (dropping the JoinHandle
            // alone would only detach them, not stop them).
            if let Some(abort) = stdout_abort {
                abort.abort();
            }
            if let Some(abort) = stderr_abort {
                abort.abort();
            }
        }

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
    spill_dir: PathBuf,
}

impl JobTool {
    const NAME: &'static str = "job";

    fn new(
        root: WorkspacePolicy,
        jobs: JobStore,
        sandbox: SandboxManager,
        spill_dir: PathBuf,
    ) -> Self {
        Self {
            root,
            jobs,
            sandbox,
            spill_dir,
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

        // Tie the process lifetime to its stored `Child`: if the JobStore is
        // dropped (app exit) the OS process is killed rather than orphaned.
        // `JobStore::shutdown` makes this deterministic on cancel/quit.
        let (mut child, job_guard) = spawn_confined(
            &self.sandbox,
            &self.root.granted_roots(),
            &command,
            &cwd,
            &policy,
            Self::NAME,
            "failed to start background command",
        )?;
        let job_id = self.jobs.reserve_id();
        let (stdout, stderr) = spill_buffers(&self.spill_dir, &job_id);
        if let Some(pipe) = child.stdout.take() {
            drop(spawn_buffer_reader(pipe, stdout.clone(), None));
        }
        if let Some(pipe) = child.stderr.take() {
            drop(spawn_buffer_reader(pipe, stderr.clone(), None));
        }

        self.jobs.insert_with_id(
            &job_id,
            JobState {
                kind: JobKind::Background,
                command: command.clone(),
                cwd: self.root.relative_display(&cwd),
                started_at: Instant::now(),
                status: JobStatus::Running,
                exit_code: None,
                sandboxed: self.sandbox.should_sandbox(&policy),
                stdout,
                stderr,
                child: Some(child),
                job_guard,
            },
        );

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
    /// Optional workspace-relative working directory, start only (absolute allowed only inside a granted root)
    cwd: Option<String>,
    /// Job id from a previous start (required for status/tail/cancel)
    job_id: Option<String>,
    /// Tail size per stream for action=tail, default 4000, max 20000
    max_chars: Option<u64>,
    /// start only: set true when the job needs network access — including binding/listening on a port (dev servers). The sandbox blocks all network by default; a declaration routes through user approval.
    #[allow(dead_code)] // consumed by the execution policy from the raw arguments
    network: Option<bool>,
    /// One short sentence for the human at the approval prompt: why this job needs what it asks for (most useful with network=true). Shown as your claim, verbatim.
    #[allow(dead_code)] // surfaced to the approval prompt from the raw arguments
    justification: Option<String>,
}

#[async_trait]
impl Tool for JobTool {
    type Params = JobParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        job_description()
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
