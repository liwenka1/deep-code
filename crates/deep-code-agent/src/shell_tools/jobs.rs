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

/// Live-output ring capacity per stream — the single source of truth for how
/// much of a stream is held in memory. The shell tool's live-stream cap
/// (`MAX_STREAMED_BYTES`) is derived from this so the two cannot drift; the
/// spill-vs-inline decisions here assume they are equal.
pub(super) const JOB_BUFFER_BYTES: usize = 128 * 1024;

/// Room left for the framing the renderer adds around the two streams:
/// `[stderr]`, the trailing `[exit N · Nms]`, the notes. Small and generous —
/// it only has to keep the sum below the budget, not predict it exactly.
const SPILL_FRAMING_RESERVE: usize = 256;

/// The one size that decides everything about a spill: below it a stream needs
/// no file, at or above it a stream keeps one.
///
/// Half the budget, not the whole budget, because stdout and stderr render
/// into ONE tool result and the runtime bounds their SUM. Judging a stream
/// against the full budget is not a bound at all — two 7k-char streams each
/// fit 12k on their own, and their 14k sum has its middle elided.
///
/// Creation and deletion read the SAME number, which is the point. They used
/// to disagree: files were created past the full budget and deleted below half
/// of it, so the band between lost output with no file at all —
/// `seq 1 1500; seq 1 1500 >&2` renders 12,872 chars against a 12,000 budget,
/// and neither 6,893-byte stream ever crossed a 12,000-byte threshold.
///
/// Compared against BYTES on the way in and CHARS on the way out, and that
/// asymmetry is deliberate: a UTF-8 char is at least one byte, so "under this
/// many bytes" implies "under this many chars" and the create side can never
/// skip a file a later char count would have wanted. The reverse slack —
/// multi-byte output writing a file it turns out not to need — is cleaned up
/// by [`SharedBuffer::discard_unreported_spill`] once both streams are known.
///
/// Well under the ring capacity, so at the moment of crossing the ring still
/// holds every byte and the file can start from byte zero.
const SPILL_JOINT_INLINE_CHARS: usize =
    (crate::runtime::tool_result::TOOL_OUTPUT_BUDGET - SPILL_FRAMING_RESERVE) / 2;

/// Alias for the create side, in the units the create side compares.
const SPILL_THRESHOLD_BYTES: usize = SPILL_JOINT_INLINE_CHARS;

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
    /// Whether the sandboxed run carried the network grant. Distinguishes the
    /// two denial notes: a no-network run's connection failure gets the
    /// network note (re-run with network=true), never the write note — whose
    /// advice, request_write_root, would point the model exactly wrong.
    pub(super) network: bool,
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

    /// Stream over: release the spill file handle. The file stays on disk —
    /// transcript references must outlive the job record — and whether it was
    /// worth writing is decided later, by [`Self::discard_unreported_spill`].
    ///
    /// Idempotent and callable from the tool side: an aborted reader task never
    /// reaches its own end-of-stream call, so the foreground path invokes this
    /// after aborting to drop the handle immediately (see `finished` in
    /// [`Spill`], which stops a still-winding-down reader from re-opening it).
    pub(super) fn finish_spill(&self) {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        if let Some(spill) = ring.spill.as_mut() {
            spill.finish();
        }
    }

    /// Remove a spill file that no rendering named, now that the caller knows
    /// it was redundant.
    ///
    /// This decision cannot be made at stream end, which is where it used to
    /// live. A reader task sees one stream, and one stream is not what the
    /// runtime bounds: a 4,100-char CJK stderr crossed the byte threshold,
    /// got its file written, then judged itself entirely visible inline
    /// (4,100 fits any budget) and deleted it — while the 9,000-char stdout
    /// beside it pushed the joint result to 13,188 chars and 5,188 of them
    /// were elided out of the only copy left. The sibling is only in hand
    /// once the result text exists, so the call belongs there.
    fn discard_unreported_spill(&self) {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        if let Some(spill) = ring.spill.as_mut() {
            spill.discard_if_unreported();
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

/// The directories deep-code itself creates below the workspace: the run
/// directory, the spill home, and `.deep-code`. Everything above them is the
/// user's own workspace path — a developer whose project lives behind a
/// symlink is not an attack, and refusing to spill there would be a
/// regression, so the checks below stop at this depth.
const OWNED_SPILL_DIRS: usize = 3;

/// Create a spill file, refusing at every step to follow a symlink.
///
/// This `create` runs in the UNCONFINED parent process, while the spill tree
/// lives inside the workspace — a directory the model may freely write. Job
/// ids are sequential and the run directory is handed to the model verbatim
/// in every truncation note, so the *next* spill path is fully predictable: a
/// symlink planted there had the parent write the command's output through
/// it, to any path the uid can reach. Planting it is an ordinary write inside
/// a granted root, so no sandbox on any platform refuses it.
///
/// Three locks. Every directory we own must be a real directory —
/// `symlink_metadata`, not `metadata`, because the latter resolves the link
/// and then answers about its target. The file is opened `O_CREAT | O_EXCL`,
/// so an existing file or a symlink at the final component fails outright
/// instead of being truncated. `O_NOFOLLOW` states the same refusal directly
/// to the kernel. A failure here disables the spill like any other I/O error:
/// the result falls back to the ring, and `info()` never names a file that
/// was not written.
fn create_spill_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "spill path has no parent")
    })?;
    crate::paths::ensure_owned_dirs(parent, OWNED_SPILL_DIRS)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 0600 because spill content is raw command output — `env` dumps,
        // registry logins, tokens a build prints. `config::write` already
        // holds this line for the API key; a world-readable copy of the same
        // secrets under the workspace would undo it.
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
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
    /// Whether the stream ended. Its own field rather than `written == 0`,
    /// which `discard_if_unreported` resets: sharing one field left the
    /// "never re-create a finished file" guard below inoperative exactly
    /// after a discard, so a later `offer` would take the create branch and
    /// write a file holding only the ring tail while `info()` reported it as
    /// the complete stream. Unreachable while the reader task is the only
    /// pusher — which is precisely why it should not depend on that.
    finished: bool,
}

impl Spill {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            written: 0,
            failed: false,
            reported: false,
            finished: false,
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
        // The stream already finished; never re-create (File::create would
        // truncate the finished file down to the backlog, and after a discard
        // it would resurrect a file nothing is going to read). Unreachable
        // while the reader task is the only pusher — insurance.
        if self.finished {
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
            let file = match create_spill_file(&self.path) {
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

    /// Stream over: drop the handle. Nothing is deleted here — see
    /// [`SharedBuffer::discard_unreported_spill`] for why that decision needs
    /// the sibling stream and therefore happens later.
    fn finish(&mut self) {
        self.file = None;
        self.finished = true;
    }

    /// Delete a file whose path never left the process: bytes crossed the
    /// threshold but the output turned out fully visible inline anyway
    /// (multi-byte output does that), so no rendering will ever name it and
    /// it would otherwise sit unreferenced until retention.
    fn discard_if_unreported(&mut self) {
        if !self.reported && !self.failed && self.written > 0 {
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

    let (rendered_chars, denial) = rendered_with_pending_denial(&out, job);
    if let Some(note) = truncation_note(job_id, job, max_chars, rendered_chars) {
        out.push('\n');
        out.push_str(&note);
    }
    if let Some(note) = denial {
        out.push('\n');
        out.push_str(note);
    }

    // Both streams are complete and the joint result has been measured, so
    // this is the first point where "that file was redundant" can be decided
    // truthfully. `truncation_note` has already claimed every file it names,
    // so whatever is still unclaimed here belongs to a result that shows
    // everything inline. Runs before `job_details`, which would otherwise
    // hand the model a path to a file with nothing in it worth reading.
    if rendered_chars <= crate::runtime::tool_result::TOOL_OUTPUT_BUDGET
        && job.stdout.omitted_len() == 0
        && job.stderr.omitted_len() == 0
    {
        job.stdout.discard_unreported_spill();
        job.stderr.discard_unreported_spill();
    }
    out
}

/// How large `out` will be once the denial note has been appended, plus the
/// note itself so the caller does not look it up twice.
///
/// Both renderings append that note AFTER asking `truncation_note` whether
/// anything was dropped, so both have to count it BEFORE asking (the notes
/// run ~350-400 chars): measuring without it put a failed sandboxed build at
/// 12,039 rendered chars against a 12,000 budget with `result_elided`
/// computed as false, so 4,039 characters went out with no truncation note,
/// no spill file and no tail hint.
///
/// Shared rather than open-coded because it was open-coded: the fix landed in
/// `shell_text_output` and `job_text_snapshot` kept passing a bare
/// `out.chars().count()`, so `job action=tail` on a failed sandboxed job went
/// on silently dropping ~4k chars while the commit that fixed it claimed both
/// renderings now agreed. One function is what makes that claim checkable.
fn rendered_with_pending_denial(out: &str, job: &JobState) -> (usize, Option<&'static str>) {
    let denial = denial_note(job);
    // `+ 1` for the newline each caller writes before the note.
    let pending = denial.map_or(0, |note| note.chars().count() + 1);
    (out.chars().count() + pending, denial)
}

/// The truncation note for both renderings, or `None` when the inline text
/// carries the complete streams.
///
/// Two honesty fixes over the old ring-only check: the note now also fires
/// when `tail_chars` alone cut content (a stream previously truncated with NO
/// indication at all), and when a spill file exists it names the absolute path
/// and size — an actionable pointer instead of the dead-end `job action=tail`
/// (whose window is capped and whose record is evicted after 32 jobs; the file
/// outlives both).
///
/// "Lost" is judged against the SMALLER of this rendering's own window and
/// [`TOOL_OUTPUT_BUDGET`], the chars a tool result keeps after the runtime
/// bounds it. Judging by the window alone made the note unreachable for the
/// band between the two: the shell layer handed over 20k chars believing them
/// all visible, and the runtime then elided the middle without a word.
fn truncation_note(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
    rendered_chars: usize,
) -> Option<String> {
    let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
    let retained = max_chars.min(budget);
    // The runtime bounds the WHOLE rendered result — both streams plus the
    // framing between them — so once that total is over budget the middle is
    // being elided no matter how modest either stream looks alone. Judging the
    // streams one at a time missed exactly that: `seq 1 1500; seq 1 1500 >&2`
    // is two 6,393-char streams, neither over the budget, ~12.8k rendered, and
    // ~4.8k characters silently dropped with no note and no file. Framing was
    // uncounted even for a single stream, which put the boundary ~17 chars off.
    let result_elided = rendered_chars > budget;
    let mut lines = Vec::new();
    let mut lost_without_file = false;
    for (label, buffer) in [("stdout", &job.stdout), ("stderr", &job.stderr)] {
        let full = buffer.text();
        let lost = buffer.omitted_len() > 0
            || full.chars().count() > retained
            || (result_elided && !full.is_empty());
        if !lost {
            continue;
        }
        match buffer.spill_info_reported() {
            // A capped file is NOT the complete stream — it stops at the
            // per-stream ceiling — so it must not be announced as one, and the
            // instruction has to stay true for what is actually on disk.
            Some(info) if info.capped => lines.push(format!(
                "[{label} truncated — first {} saved: '{}' (output exceeded the per-stream cap, so the file holds the head only); grep or read that file for the parts not shown]",
                format_bytes(info.bytes),
                info.path.display(),
            )),
            Some(info) => lines.push(format!(
                "[{label} truncated — complete stream saved: '{}' ({}); grep or read that file for the parts not shown]",
                info.path.display(),
                format_bytes(info.bytes),
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

/// The sandbox-denial note for a failed sandboxed command, or `None` when the
/// failure doesn't qualify. One decision point for both the foreground result
/// and job snapshots, so a background job's denial reads the same as a
/// foreground one.
///
/// Network is judged FIRST, and only for runs that had no network grant: an
/// offline run's connection failure is the root cause even when EPERM noise
/// is also present (git under the sandbox always carries xcrun's "Operation
/// not permitted" cache-write warnings), and the write note's advice —
/// request_write_root — would point the model exactly wrong. A run that HAD
/// the grant never gets the network note: its EPERM can only be the write
/// fence, so it falls through to the write check unchanged.
fn denial_note(job: &JobState) -> Option<&'static str> {
    if job.status != JobStatus::Failed || !job.sandboxed {
        return None;
    }
    let stderr = job.stderr.text();
    if !job.network && crate::sandbox::network_denial_signature(job.exit_code, &stderr) {
        return Some(crate::sandbox::NETWORK_DENIAL_NOTE);
    }
    crate::sandbox::write_denial_signature(job.exit_code, &stderr)
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
    let (rendered_chars, denial) = rendered_with_pending_denial(&out, job);
    if let Some(note) = truncation_note(job_id, job, max_chars, rendered_chars) {
        out.push_str(&note);
        out.push('\n');
    }
    if let Some(note) = denial {
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
mod tests;
