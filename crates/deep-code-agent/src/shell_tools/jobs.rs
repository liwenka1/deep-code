use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
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

/// Bytes past which a stream spills to disk. Equal to `MAX_OUTPUT_CHARS`
/// on purpose: UTF-8 chars are at least one byte, so a stream at most this
/// many BYTES is at most that many chars — fully visible inline — and
/// anything beyond it may lose content to `tail_chars` or the ring. Well
/// under the ring capacity, so at the moment of crossing the ring still
/// holds every byte and the file can start from byte zero.
const SPILL_THRESHOLD_BYTES: usize = super::MAX_OUTPUT_CHARS;

/// Per-stream cap on a spill file. The file keeps the HEAD (where compilers
/// put the root-cause error) and stops there; the ring independently keeps
/// the live tail, so both ends survive even a pathological firehose.
const SPILL_MAX_BYTES: u64 = 64 * 1024 * 1024;

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

    /// Allocate the next job id without inserting anything. Split from
    /// [`Self::insert_with_id`] because the spill file paths are named after
    /// the id and must exist before the output buffers (and thus the state)
    /// are constructed.
    pub(super) fn reserve_id(&self) -> String {
        format!("job_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(super) fn insert_with_id(&self, id: &str, state: JobState) {
        let mut guard = self.jobs.lock().expect("job store lock poisoned");
        guard.insert(id.to_string(), Arc::new(Mutex::new(state)));
        evict_finished_jobs(&mut guard);
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
    /// A buffer that additionally spills the complete stream to `path` once
    /// it grows past [`SPILL_THRESHOLD_BYTES`]. Below the threshold nothing
    /// touches the disk — `echo hi` must not leave a file behind.
    pub(super) fn with_spill(path: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(RingBuffer::with_spill(
            JOB_BUFFER_BYTES,
            path,
        ))))
    }

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

    /// Path and size of the spill file, when one was actually written —
    /// without the reporting side effect, so a probe cannot pin an orphan.
    #[cfg(test)]
    fn spill_info(&self) -> Option<SpillInfo> {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .spill
            .as_ref()
            .and_then(Spill::info)
    }

    /// Like `spill_info`, and additionally remembers that the path is about
    /// to leave this process (a note, a details payload): a reported file
    /// must stay valid, so stream end never removes it. Every rendering that
    /// names the path goes through here; the side-effect-free getter stays
    /// for tests.
    pub(super) fn spill_info_reported(&self) -> Option<SpillInfo> {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        let spill = ring.spill.as_mut()?;
        let info = spill.info();
        if info.is_some() {
            spill.reported = true;
        }
        info
    }

    /// Stream over: close the spill file handle. The file itself normally
    /// stays on disk — transcript references must outlive the job record —
    /// with one exception: a file nothing will ever reference (see
    /// [`Spill::finish`]) is removed instead of lingering until retention.
    fn finish_spill(&self) {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        let Some(spill) = ring.spill.as_ref() else {
            return;
        };
        if spill.written == 0 {
            return;
        }
        // "Every rendering shows the whole stream": the ring dropped nothing
        // and the char count fits the inline window (the threshold constant
        // doubles as that window, see its doc).
        let fully_inline =
            ring.omitted_len() == 0 && ring.text().chars().count() <= SPILL_THRESHOLD_BYTES;
        if let Some(spill) = ring.spill.as_mut() {
            spill.finish(fully_inline);
        }
    }
}

/// Facts about a written spill file, for the model-facing note and details.
pub(super) struct SpillInfo {
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
    pub(super) capped: bool,
}

#[derive(Debug)]
struct RingBuffer {
    bytes: VecDeque<u8>,
    capacity: usize,
    total_len: usize,
    spill: Option<Spill>,
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
            spill: None,
        }
    }

    fn with_spill(capacity: usize, path: PathBuf) -> Self {
        let mut buffer = Self::new(capacity);
        buffer.spill = Some(Spill::new(path));
        buffer
    }

    fn push(&mut self, bytes: &[u8]) {
        // Spill sees the chunk before the ring mutates: at creation time it
        // backfills from the ring, which must still hold every prior byte.
        if let Some(spill) = self.spill.as_mut() {
            spill.offer(self.total_len, &self.bytes, bytes);
        }
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

/// Byte-exact overflow copy of one stream, created lazily at the threshold.
///
/// Best-effort by design: an I/O failure disables the spill and the command
/// result falls back to the ring-only path — output capture must never make
/// the command itself fail. This is a UX layer, not a security boundary.
#[derive(Debug)]
struct Spill {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
    failed: bool,
    /// Whether the path was ever handed out (truncation note, job details).
    /// A reported file must survive stream end — the reader may come back
    /// for it any time later.
    reported: bool,
}

impl Spill {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            written: 0,
            failed: false,
            reported: false,
        }
    }

    /// Feed one incoming chunk. `prior_total` and `backlog` describe the ring
    /// BEFORE the chunk is applied; on the first threshold crossing the whole
    /// backlog (equal to the entire stream so far, see the threshold constant)
    /// is written ahead of the chunk, so the file always starts at byte zero.
    fn offer(&mut self, prior_total: usize, backlog: &VecDeque<u8>, chunk: &[u8]) {
        if self.failed || self.written >= SPILL_MAX_BYTES {
            return;
        }
        // Written but no handle = the stream already finished; never re-create
        // (File::create would truncate the finished file down to the backlog).
        // Unreachable while the reader task is the only pusher — insurance.
        if self.file.is_none() && self.written > 0 {
            return;
        }
        if self.file.is_none() {
            if prior_total + chunk.len() <= SPILL_THRESHOLD_BYTES {
                return;
            }
            debug_assert_eq!(
                prior_total,
                backlog.len(),
                "spill must be created before the ring ever drops a byte"
            );
            let created = self
                .path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::File::create(&self.path));
            let file = match created {
                Ok(file) => file,
                Err(_) => {
                    self.failed = true;
                    return;
                }
            };
            self.file = Some(file);
            let (front, back) = backlog.as_slices();
            self.write(front);
            self.write(back);
        }
        self.write(chunk);
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.failed || bytes.is_empty() {
            return;
        }
        let room = usize::try_from(SPILL_MAX_BYTES - self.written).unwrap_or(usize::MAX);
        let take = bytes.len().min(room);
        if take == 0 {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(&bytes[..take]).is_err() {
            self.failed = true;
            self.file = None;
            return;
        }
        self.written += take as u64;
    }

    fn info(&self) -> Option<SpillInfo> {
        (!self.failed && self.written > 0).then(|| SpillInfo {
            path: self.path.clone(),
            bytes: self.written,
            capped: self.written >= SPILL_MAX_BYTES,
        })
    }

    /// Stream over: drop the handle. When the whole stream turned out fully
    /// visible inline (`fully_inline`) and the path never left the process,
    /// the file is an orphan no rendering will ever name — bytes crossed the
    /// threshold but chars did not, multi-byte output does that — so it is
    /// removed here rather than left as unreferenced disk until retention.
    fn finish(&mut self, fully_inline: bool) {
        self.file = None;
        if fully_inline && !self.reported && !self.failed && self.written > 0 {
            let _ = std::fs::remove_file(&self.path);
            // Zero written = no file, for every later `info()`.
            self.written = 0;
        }
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
        // Stream over: release the spill file handle (data is already
        // written unbuffered). The file stays on disk for later reads —
        // unless it turned out to be an orphan nothing will ever name.
        buffer.finish_spill();
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

    if let Some(note) = truncation_note(job_id, job, max_chars) {
        out.push('\n');
        out.push_str(&note);
    }
    if let Some(note) = write_denial_note(job) {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// The truncation note for both renderings, or `None` when the inline text
/// carries the complete streams.
///
/// Two honesty fixes over the old ring-only check: the note now also fires
/// when `tail_chars` alone cut content (a 20k–128k stream previously
/// truncated with NO indication at all), and when a spill file exists it
/// names the absolute path and size — an actionable pointer instead of the
/// dead-end `job action=tail` (whose window is capped and whose record is
/// evicted after 32 jobs; the file outlives both).
fn truncation_note(job_id: &str, job: &JobState, max_chars: usize) -> Option<String> {
    let mut lines = Vec::new();
    let mut lost_without_file = false;
    for (label, buffer) in [("stdout", &job.stdout), ("stderr", &job.stderr)] {
        let full = buffer.text();
        let lost = buffer.omitted_len() > 0 || full.chars().count() > max_chars;
        if !lost {
            continue;
        }
        match buffer.spill_info_reported() {
            Some(info) => lines.push(format!(
                "[{label} truncated — complete stream saved: '{}' ({}{}); grep or read that file for the parts not shown]",
                info.path.display(),
                format_bytes(info.bytes),
                if info.capped { ", head only" } else { "" }
            )),
            None => lost_without_file = true,
        }
    }
    if lost_without_file {
        lines.push(format!(
            "[output truncated — fuller tail: job action=tail job_id={job_id}]"
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
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
    if let Some(note) = truncation_note(job_id, job, max_chars) {
        out.push_str(&note);
        out.push('\n');
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
        "stdout_spill_path": job.stdout.spill_info_reported().map(|info| info.path.display().to_string()),
        "stderr_spill_path": job.stderr.spill_info_reported().map(|info| info.path.display().to_string()),
    })
}

/// Human-readable byte size for the truncation note (whole units, one
/// decimal from MB up — precision is noise at these magnitudes).
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
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

    #[test]
    fn spill_below_threshold_touches_no_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("spill/job_1.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        // Exactly at the threshold is still fully visible inline — no file,
        // and crucially no directory either (`echo hi` must stay diskless).
        buffer.push(&vec![b'a'; SPILL_THRESHOLD_BYTES]);
        assert!(!path.exists());
        assert!(!tmp.path().join("spill").exists());
        assert!(buffer.spill_info().is_none());
    }

    #[test]
    fn spill_preserves_the_complete_stream_from_byte_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // Nested path exercises the lazy create_dir_all.
        let path = tmp.path().join("run-1/job_2.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        let mut expected = Vec::new();
        for index in 0..4u8 {
            // 4 × 8 KiB crosses the threshold mid-stream: the first chunks
            // arrive before any file exists and must be backfilled.
            let chunk = vec![b'a' + index; 8 * 1024];
            buffer.push(&chunk);
            expected.extend_from_slice(&chunk);
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            expected,
            "spill file must hold the byte-exact stream from byte zero"
        );
        let info = buffer.spill_info().expect("spill was written");
        assert_eq!(info.bytes, expected.len() as u64);
        assert!(!info.capped);
    }

    #[test]
    fn spill_write_caps_at_max_bytes_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("capped.log");
        let mut spill = Spill::new(path.clone());
        let backlog: VecDeque<u8> = vec![b'h'; SPILL_THRESHOLD_BYTES].into();
        spill.offer(backlog.len(), &backlog, b"-tail");
        assert!(path.exists());

        // Fake being two bytes short of the cap; a three-byte chunk must be
        // clipped to the cap, mark the spill capped, and further offers are
        // no-ops (the file keeps the HEAD; the ring keeps the live tail).
        spill.written = SPILL_MAX_BYTES - 2;
        spill.offer(0, &VecDeque::new(), b"xyz");
        let info = spill.info().expect("spill exists");
        assert_eq!(info.bytes, SPILL_MAX_BYTES);
        assert!(info.capped);
        spill.offer(0, &VecDeque::new(), b"more");
        assert_eq!(spill.written, SPILL_MAX_BYTES);
    }

    /// A stream can cross the BYTE threshold while staying under the CHAR
    /// window (multi-byte output): the file is written defensively, but every
    /// rendering shows the whole stream, so no note or details entry will
    /// ever name it. Stream end must remove that orphan instead of leaving
    /// unreferenced disk for retention to find a week later.
    #[test]
    fn unnamed_fully_inline_spill_is_removed_at_stream_end() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job_5.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        let text = "好".repeat(8_400); // 25,200 bytes, 8,400 chars
        buffer.push(text.as_bytes());
        assert!(path.exists(), "crossed the byte threshold — file written");

        buffer.finish_spill();
        assert!(!path.exists(), "an unnamed fully-inline spill is an orphan");
        assert!(
            buffer.spill_info().is_none(),
            "no later rendering may name a removed file"
        );
    }

    /// The counter-case: once a rendering handed the path out (a tail with a
    /// small window can do that mid-run), the file must survive stream end —
    /// the model may come back and grep it any time later.
    #[test]
    fn reported_spill_survives_stream_end_even_when_fully_inline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job_6.stdout.log");
        let stdout = SharedBuffer::with_spill(path.clone());
        stdout.push("好".repeat(8_400).as_bytes());
        let job = plain_job(stdout.clone());

        // A 100-char window loses content → the snapshot names the file.
        let snapshot = job_text_snapshot("job_6", &job, 100);
        assert!(
            snapshot.contains(&path.display().to_string()),
            "precondition: the path escaped to the model: {snapshot}"
        );

        stdout.finish_spill();
        assert!(
            path.exists(),
            "a named path must stay valid after the stream"
        );
    }

    fn plain_job(stdout: SharedBuffer) -> JobState {
        JobState {
            kind: JobKind::Foreground,
            command: "cargo test".to_string(),
            cwd: ".".to_string(),
            started_at: Instant::now(),
            status: JobStatus::Completed,
            exit_code: Some(0),
            sandboxed: true,
            stdout,
            stderr: SharedBuffer::default(),
            child: None,
            job_guard: None,
        }
    }

    #[test]
    fn truncation_note_names_the_spill_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job_9.stdout.log");
        let stdout = SharedBuffer::with_spill(path.clone());
        stdout.push(&vec![b'x'; SPILL_THRESHOLD_BYTES + 5]);
        let job = plain_job(stdout);

        let text = shell_text_output("job_9", &job, SPILL_THRESHOLD_BYTES);
        assert!(
            text.contains(&path.display().to_string()),
            "note must carry the absolute spill path: {text}"
        );
        assert!(
            !text.contains("job action=tail"),
            "the file pointer supersedes the tail hint: {text}"
        );
        assert!(
            job_text_snapshot("job_9", &job, SPILL_THRESHOLD_BYTES)
                .contains(&path.display().to_string())
        );
        let details = job_details("job_9", &job);
        assert_eq!(
            details["stdout_spill_path"].as_str(),
            Some(path.display().to_string().as_str())
        );
        assert!(details["stderr_spill_path"].is_null());
    }

    #[test]
    fn silent_tail_cut_now_carries_a_truncation_note() {
        // 20k–128k bytes: the ring drops nothing (omitted == 0) but
        // `tail_chars` cuts — this range previously truncated with NO note.
        let stdout = SharedBuffer::default();
        stdout.push(&vec![b'y'; 30_000]);
        let job = plain_job(stdout);
        assert_eq!(job.stdout.omitted_len(), 0, "precondition: ring intact");

        let text = shell_text_output("job_3", &job, 20_000);
        assert!(
            text.contains("job action=tail job_id=job_3"),
            "cut without a spill file must fall back to the tail hint: {text}"
        );
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
