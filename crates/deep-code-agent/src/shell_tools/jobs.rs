use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read};
use std::process::Child;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use crate::tool::ToolError;
use crate::workspace_policy::{invalid, json_string};

const JOB_BUFFER_BYTES: usize = 128 * 1024;

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

    pub(super) fn insert(&self, state: JobState) -> String {
        let id = format!("job_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        self.jobs
            .lock()
            .expect("job store lock poisoned")
            .insert(id.clone(), Arc::new(Mutex::new(state)));
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

#[derive(Debug)]
pub(super) struct JobState {
    pub(super) kind: JobKind,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) started_at: Instant,
    pub(super) timeout_deadline: Option<Instant>,
    pub(super) status: JobStatus,
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: SharedBuffer,
    pub(super) stderr: SharedBuffer,
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

pub(super) fn spawn_buffer_reader<R: Read + Send + 'static>(pipe: R, buffer: SharedBuffer) {
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

pub(super) fn cancel_job(job: &mut JobState, tool_name: &str) -> Result<(), ToolError> {
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

pub(super) fn command_output_json(job_id: &str, job: &JobState, max_output_chars: usize) -> String {
    let stdout = job.stdout.text();
    let stderr = job.stderr.text();
    let stdout_len = job.stdout.total_len();
    let stderr_len = job.stderr.total_len();
    let stdout_omitted_chars = job.stdout.omitted_len();
    let stderr_omitted_chars = job.stderr.omitted_len();
    let stdout = tail_chars(&stdout, max_output_chars);
    let stderr = tail_chars(&stderr, max_output_chars);
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

pub(super) fn job_snapshot_json(job_id: &str, job: &JobState, max_chars: usize) -> String {
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
    use super::*;

    #[test]
    fn job_output_buffer_is_bounded_with_omitted_count() {
        let buffer = SharedBuffer::default();
        buffer.push(&vec![b'a'; JOB_BUFFER_BYTES + 10]);

        assert_eq!(buffer.total_len(), JOB_BUFFER_BYTES + 10);
        assert_eq!(buffer.omitted_len(), 10);
        assert_eq!(buffer.text().len(), JOB_BUFFER_BYTES);
    }
}
