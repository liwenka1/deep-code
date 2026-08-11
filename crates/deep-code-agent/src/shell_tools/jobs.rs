use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Child;

use crate::tool::ToolError;
use crate::workspace_policy::invalid;

const JOB_BUFFER_BYTES: usize = 128 * 1024;

/// How many job records to keep. Every `shell` call — foreground included —
/// inserts one so `job action=status/tail` can inspect it afterwards, and
/// nothing ever removed one, so a long session accumulated a record per command
/// (each owning two output buffers, plus a Job Object handle on Windows) until
/// the process exited. Only finished jobs are evicted, so this bounds memory
/// without ever dropping a running background job.
const MAX_RETAINED_JOBS: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct JobStore {
    next_id: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobState>>>>>,
}

impl JobStore {
    /// Kill every still-running background child. Called on runtime shutdown so
    /// a cancelled or quit session doesn't orphan long-running processes (dev
    /// servers, watchers) that keep holding ports. `kill_on_drop` is the
    /// backstop; this makes the kill immediate instead of waiting for the store
    /// to drop.
    pub fn shutdown(&self) {
        let guard = self.jobs.lock().expect("job store lock poisoned");
        for state_arc in guard.values() {
            let Ok(mut state) = state_arc.lock() else {
                continue;
            };
            if state.status == JobStatus::Running
                && let Some(child) = state.child.as_mut()
            {
                kill_process_tree(child);
                // Dropping the Windows Job Object guard is what kills the whole
                // tree there (`kill_process_tree`'s `start_kill` only reaps the
                // direct child); `None` on Unix, so this is a no-op.
                state.job_guard = None;
                state.status = JobStatus::Cancelled;
            }
        }
    }

    pub(super) fn insert(&self, state: JobState) -> String {
        let id = format!("job_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let mut guard = self.jobs.lock().expect("job store lock poisoned");
        guard.insert(id.clone(), Arc::new(Mutex::new(state)));
        evict_finished_jobs(&mut guard);
        id
    }

    pub(super) fn get(&self, id: &str, tool_name: &str) -> Result<Arc<Mutex<JobState>>, ToolError> {
        self.jobs
            .lock()
            .expect("job store lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(tool_name, format!("unknown job_id '{id}'")))
    }
}

/// Drop the oldest *finished* records once the store exceeds
/// [`MAX_RETAINED_JOBS`]. Running jobs are never evicted — their entry owns the
/// handle `shutdown` needs to kill the process tree — so the store can still
/// exceed the cap while many jobs run at once; it converges as they finish.
///
/// Takes the already-held map guard. Locking each `JobState` while holding the
/// map lock matches `shutdown`'s order (map → state); nothing acquires them the
/// other way round.
fn evict_finished_jobs(jobs: &mut HashMap<String, Arc<Mutex<JobState>>>) {
    if jobs.len() <= MAX_RETAINED_JOBS {
        return;
    }
    let excess = jobs.len() - MAX_RETAINED_JOBS;
    let mut finished: Vec<(u64, String)> = jobs
        .iter()
        .filter(|(_, state)| {
            state
                .lock()
                .is_ok_and(|state| state.status != JobStatus::Running)
        })
        .map(|(id, _)| (job_sequence(id), id.clone()))
        .collect();
    finished.sort_unstable();
    for (_, id) in finished.into_iter().take(excess) {
        jobs.remove(&id);
    }
}

/// Monotonic counter out of a `job_<n>` id, for oldest-first ordering.
fn job_sequence(id: &str) -> u64 {
    id.rsplit('_')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[derive(Debug)]
pub(super) struct JobState {
    pub(super) kind: JobKind,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) started_at: Instant,
    pub(super) status: JobStatus,
    pub(super) exit_code: Option<i32>,
    /// Whether the command ran under the OS sandbox. A failure only qualifies
    /// as a possible write-boundary denial when it did — an unconfined run's
    /// EPERM is a plain permission problem, not the granted-roots fence.
    pub(super) sandboxed: bool,
    pub(super) stdout: SharedBuffer,
    pub(super) stderr: SharedBuffer,
    /// Present for background jobs; foreground children are owned by the
    /// running tool future and only their terminal state lands here.
    pub(super) child: Option<Child>,
    /// OS sandbox guard tied to the child (Windows Job Object); dropping it with
    /// the job kills the process tree. `None` on macOS/Linux (confined pre-spawn).
    /// Held purely for its `Drop` — never read.
    #[allow(dead_code)]
    pub(super) job_guard: Option<crate::sandbox::SandboxGuard>,
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

impl JobStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SharedBuffer(Arc<Mutex<RingBuffer>>);

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

    pub(super) fn text(&self) -> String {
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
            // Grown on demand, not reserved up front: `with_capacity` committed
            // the full 128 KiB per buffer (256 KiB per `shell` call, both
            // streams) even for `echo hi`, and every call kept a record.
            bytes: VecDeque::new(),
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

/// Kill `child` and, on Unix, its whole process group (children are spawned
/// as group leaders, see `spawn_confined`), so grandchildren spawned by the
/// shell don't outlive the job. On non-Unix `start_kill` only reaps the direct
/// child; killing the tree there relies on dropping the job's Windows Job
/// Object guard (see the `job_guard = None` at each kill site).
pub(super) fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group; the child may already have
        // exited, in which case the signal harmlessly fails.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let _ = child.start_kill();
}

pub(super) type ChunkFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Drain one child pipe into the ring buffer, optionally forwarding each
/// chunk (live streaming for foreground shells; background jobs pass `None`
/// because their parent turn has already ended). Returns the reader task so
/// the foreground shell can await it (EOF) before reading the buffer.
pub(super) fn spawn_buffer_reader<R>(
    mut pipe: R,
    buffer: SharedBuffer,
    on_chunk: Option<ChunkFn>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) => break,
                Ok(bytes) => {
                    buffer.push(&chunk[..bytes]);
                    if let Some(on_chunk) = &on_chunk {
                        on_chunk(&chunk[..bytes]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Fold a finished child's exit status into the job record (background jobs;
/// foreground terminal states are written by the shell tool itself).
pub(super) fn refresh_job(job: &mut JobState) {
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
    }
}

/// Kill a running job's child and mark it cancelled. The kill signal is sent
/// under the lock; waiting for the exit happens outside it.
pub(super) async fn cancel_job(
    state: &Arc<Mutex<JobState>>,
    tool_name: &str,
) -> Result<(), ToolError> {
    let child = {
        let mut job = state.lock().expect("job lock poisoned");
        if job.status != JobStatus::Running {
            return Ok(());
        }
        let mut child = job.child.take();
        if let Some(child) = child.as_mut() {
            kill_process_tree(child);
        }
        // Windows: dropping the Job Object guard kills the whole tree (`None`
        // on Unix, where the child was confined into its own group pre-spawn).
        job.job_guard = None;
        child
    };
    let exit_code = match child {
        Some(mut child) => child
            .wait()
            .await
            .map_err(|error| {
                ToolError::exec_failed(tool_name, format!("failed to wait after cancel: {error}"))
            })?
            .code(),
        None => None,
    };
    let mut job = state.lock().expect("job lock poisoned");
    job.exit_code = exit_code;
    job.status = JobStatus::Cancelled;
    Ok(())
}

/// Model-facing plain-text output for a finished foreground shell command.
pub(super) fn shell_text_output(job_id: &str, job: &JobState, max_chars: usize) -> String {
    let stdout = tail_chars(&job.stdout.text(), max_chars);
    let stderr = tail_chars(&job.stderr.text(), max_chars);
    let elapsed = format_elapsed(job.started_at.elapsed().as_millis() as u64);

    let mut out = String::new();
    if stdout.is_empty() && stderr.is_empty() {
        out.push_str("(no output)\n");
    } else {
        if !stdout.is_empty() {
            out.push_str(&stdout);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        if !stderr.is_empty() {
            out.push_str("[stderr]\n");
            out.push_str(&stderr);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    match job.status {
        JobStatus::TimedOut => out.push_str(&format!(
            "[timed out after {elapsed} — killed; use `job action=start` for long-running processes]"
        )),
        JobStatus::Cancelled => out.push_str(&format!("[cancelled after {elapsed}]")),
        _ => out.push_str(&format!(
            "[exit {} · {elapsed}]",
            job.exit_code.map_or_else(|| "?".to_string(), |code| code.to_string())
        )),
    }

    if job.stdout.omitted_len() > 0 || job.stderr.omitted_len() > 0 {
        out.push_str(&format!(
            "\n[output truncated — full tail: job action=tail job_id={job_id}]"
        ));
    }
    if let Some(note) = write_denial_note(job) {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// The boundary-denial note for a failed sandboxed command whose stderr looks
/// like the OS refusing a write, or `None` when the failure doesn't qualify.
/// One decision point for both the foreground result and job snapshots, so a
/// background job's denial reads the same as a foreground one.
fn write_denial_note(job: &JobState) -> Option<&'static str> {
    (job.status == JobStatus::Failed
        && job.sandboxed
        && crate::sandbox::write_denial_signature(job.exit_code, &job.stderr.text()))
    .then_some(crate::sandbox::WRITE_DENIAL_NOTE)
}

/// Model-facing plain-text snapshot for job status/tail.
pub(super) fn job_text_snapshot(job_id: &str, job: &JobState, max_chars: usize) -> String {
    let kind = match job.kind {
        JobKind::Foreground => "foreground",
        JobKind::Background => "background",
    };
    let exit = job
        .exit_code
        .map_or_else(String::new, |code| format!(" · exit {code}"));
    let mut out = format!(
        "{job_id} ({kind}) — {}{exit} · {} · cmd: {}\n",
        job.status.as_str(),
        format_elapsed(job.started_at.elapsed().as_millis() as u64),
        job.command
    );
    let stdout = tail_chars(&job.stdout.text(), max_chars);
    let stderr = tail_chars(&job.stderr.text(), max_chars);
    if !stdout.is_empty() {
        out.push_str("[stdout]\n");
        out.push_str(&stdout);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if stdout.is_empty() && stderr.is_empty() {
        out.push_str("(no output)\n");
    }
    if let Some(note) = write_denial_note(job) {
        out.push_str(note);
        out.push('\n');
    }
    out
}

/// UI-facing structured details for any job-backed result.
pub(super) fn job_details(job_id: &str, job: &JobState) -> Value {
    json!({
        "job_id": job_id,
        "kind": job.kind,
        "command": job.command,
        "cwd": job.cwd,
        "status": job.status,
        "exit_code": job.exit_code,
        "duration_ms": job.started_at.elapsed().as_millis() as u64,
        "stdout_len": job.stdout.total_len(),
        "stderr_len": job.stderr.total_len(),
        "stdout_truncated": job.stdout.omitted_len() > 0,
        "stderr_truncated": job.stderr.omitted_len() > 0,
    })
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1_000.0)
    }
}

pub(super) fn tail_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    value.chars().skip(count - max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_output_buffer_is_bounded_with_omitted_count() {
        let buffer = SharedBuffer::default();
        buffer.push(&vec![b'a'; JOB_BUFFER_BYTES + 10]);

        assert_eq!(buffer.total_len(), JOB_BUFFER_BYTES + 10);
        assert_eq!(buffer.omitted_len(), 10);
        assert_eq!(buffer.text().len(), JOB_BUFFER_BYTES);
    }

    fn finished_job(status: JobStatus, sandboxed: bool, stderr_text: &str) -> JobState {
        let stderr = SharedBuffer::default();
        stderr.push(stderr_text.as_bytes());
        JobState {
            kind: JobKind::Foreground,
            command: "printf x > /outside/f".to_string(),
            cwd: ".".to_string(),
            started_at: Instant::now(),
            status,
            exit_code: Some(if status == JobStatus::Completed { 0 } else { 1 }),
            sandboxed,
            stdout: SharedBuffer::default(),
            stderr,
            child: None,
            job_guard: None,
        }
    }

    /// The denial note reaches the model through BOTH renderings — the
    /// foreground result and the job status/tail snapshot — and only when the
    /// failure was a sandboxed run whose stderr carries a denial signature.
    /// The exact constant matters: the runtime classifies boundary denials by
    /// finding it in the content.
    #[test]
    fn denial_note_lands_in_shell_output_and_job_snapshot_only_when_it_applies() {
        let denied = finished_job(JobStatus::Failed, true, "sh: Operation not permitted");
        assert!(
            shell_text_output("job_1", &denied, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE)
        );
        assert!(
            job_text_snapshot("job_1", &denied, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE)
        );

        // Same failure without the sandbox: a plain permission problem — the
        // granted-roots fence was not involved, so no note.
        let bare = finished_job(JobStatus::Failed, false, "sh: Operation not permitted");
        assert!(
            !shell_text_output("job_2", &bare, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE)
        );

        // Sandboxed failure with an unrelated stderr: no note.
        let unrelated = finished_job(JobStatus::Failed, true, "error: expected `;`");
        assert!(
            !shell_text_output("job_3", &unrelated, 4096)
                .contains(crate::sandbox::WRITE_DENIAL_NOTE)
        );

        // Success never carries the note, whatever stderr says.
        let ok = finished_job(
            JobStatus::Completed,
            true,
            "warning: Operation not permitted",
        );
        assert!(!shell_text_output("job_4", &ok, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE));
    }
}
