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
    fn finish_spill(&self) {
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
    let owned: Vec<_> = path.ancestors().skip(1).take(OWNED_SPILL_DIRS).collect();
    for dir in owned.into_iter().rev() {
        ensure_real_dir(dir)?;
    }
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

/// The directory must exist and be a real directory, or be created as one.
fn ensure_real_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "spill directory is a symlink or a file",
        )),
        Err(_) => create_private_dir(dir),
    }
}

fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    // `mut` is consumed by the unix-only `mode` call below; Windows builds
    // see it unused and clippy runs with `-D warnings` there.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => Ok(()),
        // Lost the race to the job's other stream — fine, as long as what
        // landed there is a real directory and not a link planted meanwhile.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(dir) {
                Ok(meta) if meta.is_dir() => Ok(()),
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
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

    /// The spill tree sits inside the workspace, so the model can plant a
    /// symlink at the *next* job's path — the run directory is disclosed in
    /// every truncation note and job ids are sequential. Writing the file is
    /// done by the unconfined parent, so following that link would write the
    /// command's own output to any path the uid can reach, with every sandbox
    /// bypassed. Both spellings of the plant are refused, and the failure is
    /// silent-and-honest: no file is claimed.
    ///
    /// Runs on Windows too. The doc above says "no sandbox on any platform
    /// refuses it", yet this stayed `#[cfg(unix)]` and left the one platform
    /// with no filesystem confinement at all completely uncovered. Both locks
    /// that matter here are cross-platform: `ensure_real_dir` uses
    /// `symlink_metadata`, whose `is_dir()` is false for a Windows directory
    /// symlink, and `create_new(true)` refuses an existing entry at the final
    /// component. (`O_NOFOLLOW` and the 0600 mode are the unix-only extras.)
    #[test]
    fn spill_refuses_to_write_through_a_planted_symlink() {
        for plant_the_directory in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let victim = outside.path().join("victim.txt");
            std::fs::write(&victim, "ORIGINAL\n").unwrap();

            let run = tmp.path().join(".deep-code/spill/run-1");
            let path = run.join("job_1.stdout.log");
            if plant_the_directory {
                std::fs::create_dir_all(run.parent().unwrap()).unwrap();
                if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &run) {
                    return;
                }
            } else {
                std::fs::create_dir_all(&run).unwrap();
                if !crate::test_symlinks::symlink_file_for_test(&victim, &path) {
                    return;
                }
            }

            let buffer = SharedBuffer::with_spill(path);
            buffer.push(&vec![b'a'; SPILL_THRESHOLD_BYTES + 1]);

            assert_eq!(
                std::fs::read_to_string(&victim).unwrap(),
                "ORIGINAL\n",
                "spill overwrote a file outside the workspace \
                 (directory planted: {plant_the_directory})"
            );
            // A planted *directory* link cannot truncate `victim.txt` past
            // `O_EXCL`, but it can still land the command's output beside it,
            // in a directory the attacker chose — `~/.ssh`, `/etc/cron.d`.
            let leaked: Vec<_> = std::fs::read_dir(outside.path())
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|name| name != "victim.txt")
                .collect();
            assert!(
                leaked.is_empty(),
                "spill wrote outside the workspace through a planted symlink \
                 (directory planted: {plant_the_directory}): {leaked:?}"
            );
            assert!(
                buffer.spill_info().is_none(),
                "a refused spill must not claim a file"
            );
        }
    }

    /// A finished stream never re-creates its file, even after the orphan
    /// discard reset `written` to zero. The guard used to read `written > 0`,
    /// which the discard itself falsified — so a late chunk would take the
    /// create branch and write a file holding only the ring tail, while
    /// `info()` announced it as the complete stream. Unreachable with one
    /// pusher, which is exactly why the guard should not rely on that.
    #[test]
    fn a_finished_spill_never_reopens_even_after_its_file_was_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".deep-code/spill/run-1/job_1.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        buffer.push(&vec![b'a'; SPILL_THRESHOLD_BYTES + 1]);
        assert!(path.exists(), "precondition: a file was written");

        buffer.finish_spill();
        buffer.discard_unreported_spill();
        assert!(!path.exists(), "precondition: the orphan was discarded");

        buffer.push(&vec![b'b'; SPILL_THRESHOLD_BYTES + 1]);
        assert!(
            !path.exists(),
            "a finished stream resurrected its spill file"
        );
        assert!(
            buffer.spill_info().is_none(),
            "and must not claim one either"
        );
    }

    /// Spill content is raw command output — `env`, registry logins, tokens a
    /// build prints. It must not be readable by other users on the host.
    #[cfg(unix)]
    #[test]
    fn spill_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".deep-code/spill/run-1/job_1.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        buffer.push(&vec![b'a'; SPILL_THRESHOLD_BYTES + 1]);

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "spill file must not be group/world readable"
        );
        assert_eq!(
            dir_mode, 0o700,
            "spill run dir must not be group/world readable"
        );
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

    /// A stream can cross the BYTE threshold while staying well under the CHAR
    /// window (multi-byte output): the file is written defensively, but every
    /// rendering shows the whole stream, so no note or details entry will ever
    /// name it. That orphan must be removed rather than left as unreferenced
    /// disk for retention to find a week later.
    ///
    /// Removed when the RESULT is rendered, not at stream end. At stream end
    /// only one stream is known, and one stream is not what the runtime
    /// bounds — see the asymmetric case below.
    #[test]
    fn unnamed_fully_inline_spill_is_removed_once_the_result_is_rendered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job_5.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        buffer.push("好".repeat(5_000).as_bytes()); // 15,000 bytes, 5,000 chars
        assert!(path.exists(), "crossed the byte threshold — file written");

        let job = plain_job(buffer.clone());
        buffer.finish_spill();
        assert!(path.exists(), "stream end alone decides nothing");

        let rendered = shell_text_output("job_5", &job, 20_000);
        assert!(
            !rendered.contains(&path.display().to_string()),
            "precondition: nothing named the file: {rendered}"
        );
        assert!(!path.exists(), "an unnamed fully-inline spill is an orphan");
        assert!(
            buffer.spill_info().is_none(),
            "no later rendering may name a removed file"
        );
    }

    /// The canonical two-stream case, end to end: neither stream is remarkable
    /// on its own, their sum is over budget, and a file must exist.
    ///
    /// This exact command was in the constant's own doc comment as the bug it
    /// was meant to prevent, while the code still created files against the
    /// full budget: two 6,893-byte streams, a 12,872-char result, a 12,000-char
    /// budget — and no file on either stream, so the only thing the model was
    /// told was to try `job action=tail`, whose window is capped at the same
    /// budget and cannot show the head either.
    #[test]
    fn a_joint_overflow_always_leaves_a_file_to_read() {
        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("job_j.stdout.log");
        let err_path = tmp.path().join("job_j.stderr.log");
        let stdout = SharedBuffer::with_spill(out_path.clone());
        let stderr = SharedBuffer::with_spill(err_path.clone());
        // `seq 1 1500` twice: 6,893 bytes each, neither near the budget alone.
        let stream: String = (1..=1500).map(|n| format!("{n}\n")).collect();
        assert!(stream.len() < budget, "precondition: one stream fits");
        stdout.push(stream.as_bytes());
        stderr.push(stream.as_bytes());

        let job = two_stream_job(stdout.clone(), stderr.clone());
        stdout.finish_spill();
        stderr.finish_spill();
        let rendered = shell_text_output("job_j", &job, 20_000);

        assert!(
            rendered.chars().count() > budget,
            "precondition: the pair is over budget"
        );
        assert!(
            rendered.contains(&out_path.display().to_string())
                || rendered.contains(&err_path.display().to_string()),
            "an over-budget result must name a file the model can actually read, \
             not just suggest `job action=tail`: {rendered}"
        );
    }

    /// The write-denial note is 386 characters and is appended to the result
    /// AFTER the truncation note has been decided, so its length has to be
    /// counted before the decision, not after. Measuring without it put a
    /// failed sandboxed build at 12,039 rendered chars against a 12,000-char
    /// budget with `result_elided` computed as false: 4,039 characters were
    /// elided by the runtime with no note, no file and no tail hint.
    #[test]
    fn the_denial_note_counts_toward_the_budget_it_pushes_the_result_past() {
        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        let tmp = tempfile::tempdir().unwrap();
        let err_path = tmp.path().join("job_d.stderr.log");
        let stderr = SharedBuffer::with_spill(err_path.clone());
        // Just under budget on its own; the denial note is what tips it over.
        let denial_len = crate::sandbox::WRITE_DENIAL_NOTE.chars().count();
        stderr.push(b"mkdir: /etc/x: Operation not permitted\n");
        stderr.push(&vec![b'e'; budget - denial_len / 2]);

        let mut job = two_stream_job(SharedBuffer::default(), stderr.clone());
        job.status = JobStatus::Failed;
        job.exit_code = Some(1);
        job.sandboxed = true;
        stderr.finish_spill();

        let rendered = shell_text_output("job_d", &job, 20_000);
        assert!(
            rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE),
            "precondition: this job carries the denial note"
        );
        assert!(
            rendered.chars().count() > budget,
            "precondition: the result is over budget once the note is on it"
        );
        assert!(
            rendered.contains(&err_path.display().to_string()),
            "over budget means content is being elided — say so and name the file: {rendered}"
        );
    }

    /// The same budget, through the OTHER rendering — `job action=tail` /
    /// `job action=status` rather than the shell result.
    ///
    /// The fix that taught the budget about the 386-char denial note landed in
    /// `shell_text_output` only; `job_text_snapshot` kept measuring a bare
    /// `out.chars().count()` and appending the note afterwards, so a failed
    /// sandboxed job inspected with `job action=tail` still crossed the budget
    /// with `result_elided` false — no truncation note, no file named, and the
    /// runtime then elided ~4k chars out of the only copy. The commit that
    /// fixed the sibling claimed both renderings had been unified.
    #[test]
    fn the_denial_note_counts_toward_the_budget_in_the_job_snapshot_too() {
        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        let tmp = tempfile::tempdir().unwrap();
        let err_path = tmp.path().join("job_t.stderr.log");
        let stderr = SharedBuffer::with_spill(err_path.clone());
        let denial_len = crate::sandbox::WRITE_DENIAL_NOTE.chars().count();
        stderr.push(b"mkdir: /etc/x: Operation not permitted\n");
        stderr.push(&vec![b'e'; budget - denial_len / 2]);

        let mut job = two_stream_job(SharedBuffer::default(), stderr.clone());
        job.status = JobStatus::Failed;
        job.exit_code = Some(1);
        job.sandboxed = true;
        stderr.finish_spill();

        let rendered = job_text_snapshot("job_t", &job, 20_000);
        assert!(
            rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE),
            "precondition: this job carries the denial note"
        );
        assert!(
            rendered.chars().count() > budget,
            "precondition: the snapshot is over budget once the note is on it"
        );
        assert!(
            rendered.contains(&err_path.display().to_string()),
            "over budget means content is being elided — say so and name the file: {rendered}"
        );
    }

    /// Two streams that each look harmless keep their files, because what the
    /// runtime bounds is their SUM.
    ///
    /// The asymmetric pair is the case a per-stream rule cannot get right: a
    /// 4,100-char CJK stderr crosses the byte threshold, then judges itself
    /// entirely visible inline and deletes its file — while the 9,000-char
    /// stdout beside it pushes the rendered result past the budget and the
    /// middle is elided out of the only copy left. Both files must survive.
    #[test]
    fn an_asymmetric_pair_keeps_both_files_when_their_sum_is_over_budget() {
        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("job_6.stdout.log");
        let err_path = tmp.path().join("job_6.stderr.log");
        let stdout = SharedBuffer::with_spill(out_path.clone());
        let stderr = SharedBuffer::with_spill(err_path.clone());
        stdout.push(&vec![b'y'; 9_000]); // 9,000 chars, comfortably under budget
        stderr.push("好".repeat(4_100).as_bytes()); // 12,300 bytes, 4,100 chars
        assert!(out_path.exists() && err_path.exists(), "both files written");

        let job = two_stream_job(stdout.clone(), stderr.clone());
        stdout.finish_spill();
        stderr.finish_spill();

        let rendered = shell_text_output("job_6", &job, 20_000);
        assert!(
            rendered.chars().count() > budget,
            "precondition: the pair renders over budget"
        );
        assert!(
            out_path.exists() && err_path.exists(),
            "neither file may be deleted when the joint result loses content"
        );
        for path in [&out_path, &err_path] {
            assert!(
                rendered.contains(&path.display().to_string()),
                "and the note must name {}: {rendered}",
                path.display()
            );
        }
    }

    /// The band between what a tool result actually retains
    /// (`TOOL_OUTPUT_BUDGET`) and the shell layer's own wider window.
    ///
    /// Content past the budget has its middle elided by the runtime, so it IS
    /// lost and needs both a file and a note pointing at it. Keying the spill
    /// decisions to the wider window made this whole band silent: multi-byte
    /// output produced a complete file that stream end then deleted as
    /// redundant, and ASCII output in the band produced no file at all —
    /// unrecoverable, and with no note either, which is precisely the failure
    /// spill exists to end.
    #[test]
    fn output_past_the_retained_budget_keeps_its_file_and_names_it() {
        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        let tmp = tempfile::tempdir().unwrap();

        // Multi-byte: crosses the BYTE threshold long before the char count,
        // so this is the case that used to be written and then removed.
        let cjk_path = tmp.path().join("job_7.stdout.log");
        let cjk = SharedBuffer::with_spill(cjk_path.clone());
        cjk.push("好".repeat(budget + 1_000).as_bytes());
        let job = plain_job(cjk.clone());
        assert_eq!(job.stdout.omitted_len(), 0, "precondition: ring intact");
        cjk.finish_spill();
        assert!(
            cjk_path.exists(),
            "past the retained budget the stream is NOT fully inline — keep the file"
        );
        let text = shell_text_output("job_7", &job, 20_000);
        assert!(
            text.contains(&cjk_path.display().to_string()),
            "and the note must name it rather than stay silent: {text}"
        );

        // ASCII in the same band: bytes == chars, so it sat under the old
        // byte threshold and no file was ever created.
        let ascii_path = tmp.path().join("job_8.stdout.log");
        let ascii = SharedBuffer::with_spill(ascii_path.clone());
        ascii.push(&vec![b'y'; budget + 3_000]);
        assert!(
            ascii_path.exists(),
            "ASCII output past the retained budget must be archived too"
        );
        let ascii_job = plain_job(ascii.clone());
        ascii.finish_spill();
        assert!(ascii_path.exists(), "and must survive stream end");
        let ascii_text = shell_text_output("job_8", &ascii_job, 20_000);
        assert!(
            ascii_text.contains(&ascii_path.display().to_string()),
            "with a note naming it: {ascii_text}"
        );
    }

    /// A capped file is not the whole stream, so the note must not call it
    /// one — the model is told to read the file "for the parts not shown",
    /// and past the cap those parts are not there to read.
    ///
    /// The job's buffer carries the capped spill, so the rendering really takes
    /// the capped arm. An earlier version of this test built a standalone
    /// `Spill` and then rendered a job whose buffer had no spill at all: the
    /// assertion landed on the `lost_without_file` fallback and passed no
    /// matter what the capped arm said. Reverting the wording left the whole
    /// suite green.
    #[test]
    fn capped_spill_note_does_not_claim_the_complete_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job_9.stdout.log");
        let buffer = SharedBuffer::with_spill(path.clone());
        buffer.push(&vec![
            b'z';
            crate::runtime::tool_result::TOOL_OUTPUT_BUDGET + 5
        ]);
        assert!(path.exists(), "precondition: a file was written");
        // Reach the cap without writing 64 MB.
        buffer
            .0
            .lock()
            .expect("output buffer lock poisoned")
            .spill
            .as_mut()
            .expect("precondition: the buffer has a spill")
            .written = SPILL_MAX_BYTES;

        let job = plain_job(buffer);
        let text = shell_text_output("job_9", &job, 20_000);
        assert!(
            text.contains("the file holds the head only"),
            "a capped file must be described as a head, not a whole stream: {text}"
        );
        assert!(
            !text.contains("complete stream saved"),
            "the uncapped wording must not appear for a capped file: {text}"
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

    fn two_stream_job(stdout: SharedBuffer, stderr: SharedBuffer) -> JobState {
        JobState {
            stderr,
            ..plain_job(stdout)
        }
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
            network: false,
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

    /// Two streams that each fit the budget can still overflow it together, and
    /// that overflow used to be reported by nobody.
    ///
    /// `seq 1 1500; seq 1 1500 >&2` is the shape: 6,393 characters per stream,
    /// neither over the 12,000-char budget, ~12.8k rendered into one result,
    /// ~4.8k characters elided out of the middle by the runtime — with no note,
    /// no spill file, and not even the `job action=tail` fallback. Judging the
    /// streams one at a time cannot see it; the note is keyed to the rendered
    /// total instead.
    #[test]
    fn two_streams_that_each_fit_the_budget_still_report_their_joint_loss() {
        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        stdout.push(&vec![b'o'; 6_393]);
        stderr.push(&vec![b'e'; 6_393]);
        let mut job = plain_job(stdout);
        job.stderr = stderr;

        let budget = crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
        assert!(
            job.stdout.text().chars().count() < budget
                && job.stderr.text().chars().count() < budget,
            "precondition: neither stream alone exceeds what a result retains"
        );
        assert_eq!(job.stdout.omitted_len(), 0, "precondition: ring intact");

        let text = shell_text_output("job_7", &job, 20_000);
        assert!(
            text.chars().count() > budget,
            "precondition: together they overflow the result budget"
        );
        assert!(
            text.contains("job action=tail job_id=job_7"),
            "a joint overflow with no spill file must still point somewhere: {text}"
        );
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
            network: false,
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

    /// The misdirection-chain regression: a no-network run's failure must get
    /// the NETWORK note, never the write note — even when EPERM noise is also
    /// present. Both stderr shapes below were captured under the real Seatbelt
    /// profile: git under the sandbox always carries xcrun's "Operation not
    /// permitted" cache-write warnings next to its real DNS error, and a port
    /// bind fails as a bare PermissionError with "bind" only in the traceback.
    /// Before this note existed, both matched the write signature and the
    /// model was told to request_write_root — exactly wrong.
    #[test]
    fn offline_network_failures_get_the_network_note_not_the_write_note() {
        let git_offline = finished_job(
            JobStatus::Failed,
            true,
            "git: error: couldn't create cache file '/tmp/xcrun_db-x' (errno=Operation not \
             permitted)\nfatal: unable to access 'https://github.com/x/y.git/': Could not \
             resolve host: github.com",
        );
        let rendered = shell_text_output("job_5", &git_offline, 8192);
        assert!(
            rendered.contains(crate::sandbox::NETWORK_DENIAL_NOTE),
            "offline DNS failure must get the network note: {rendered}"
        );
        assert!(
            !rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE),
            "the xcrun EPERM noise must not misdirect to request_write_root: {rendered}"
        );

        let bind_offline = finished_job(
            JobStatus::Failed,
            true,
            "    import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0))\nPermissionError: \
             [Errno 1] Operation not permitted",
        );
        let rendered = shell_text_output("job_6", &bind_offline, 8192);
        assert!(
            rendered.contains(crate::sandbox::NETWORK_DENIAL_NOTE),
            "a socket EPERM in a no-network run is the network fence: {rendered}"
        );
        assert!(!rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE));

        // Both renderings agree, same as the write note.
        assert!(
            job_text_snapshot("job_6", &bind_offline, 8192)
                .contains(crate::sandbox::NETWORK_DENIAL_NOTE)
        );
    }

    /// A run that HAD the network grant never gets the network note: its
    /// EPERM can only be the write fence (or a real remote-side problem), so
    /// the write signature keeps judging it — and a write denial in a plain
    /// offline run (no network words in stderr) keeps its note unchanged.
    #[test]
    fn granted_runs_and_plain_write_denials_keep_the_write_note() {
        let mut granted = finished_job(
            JobStatus::Failed,
            true,
            "PermissionError: [Errno 1] Operation not permitted while calling bind",
        );
        granted.network = true;
        let rendered = shell_text_output("job_7", &granted, 8192);
        assert!(
            !rendered.contains(crate::sandbox::NETWORK_DENIAL_NOTE),
            "a granted run must not be told it lacks the grant: {rendered}"
        );
        assert!(
            rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE),
            "EPERM under a granted run falls through to the write check: {rendered}"
        );

        // The pre-existing shape: offline run, write EPERM, no network words.
        let write_denied = finished_job(
            JobStatus::Failed,
            true,
            "mkdir: /etc/x: Operation not permitted",
        );
        let rendered = shell_text_output("job_8", &write_denied, 8192);
        assert!(rendered.contains(crate::sandbox::WRITE_DENIAL_NOTE));
        assert!(!rendered.contains(crate::sandbox::NETWORK_DENIAL_NOTE));
    }
}
