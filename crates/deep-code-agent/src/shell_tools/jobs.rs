use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};

use crate::tool::ToolError;
use crate::workspace_policy::invalid;

mod render;
#[cfg(test)]
pub(super) use render::{job_details, job_text_snapshot, shell_text_output};
pub(super) use render::{job_output, job_page_output};

/// Live-output ring capacity per stream — the single source of truth for how
/// much of a stream is held in memory. The shell tool's live-stream cap
/// (`MAX_STREAMED_BYTES`) is derived from this so the two cannot drift. (The
/// spill threshold below is independent of it: that one derives from the tool
/// output budget.)
pub(super) const JOB_BUFFER_BYTES: usize = 128 * 1024;

/// Room left for the framing the renderer adds around the two streams:
/// `[stderr]`, the trailing `[exit N · Nms]`, the notes. Small and generous —
/// it only has to keep the sum below the budget, not predict it exactly.
const SPILL_FRAMING_RESERVE: usize = 2048;

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
pub(super) const INLINE_STREAM_CHARS: usize =
    (crate::runtime::tool_result::TOOL_OUTPUT_BUDGET - SPILL_FRAMING_RESERVE) / 2;

/// Alias for the create side, in the units the create side compares.
const SPILL_THRESHOLD_BYTES: usize = INLINE_STREAM_CHARS;

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

#[derive(Debug, Clone)]
pub struct JobStore {
    owner: Arc<str>,
    next_id: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobState>>>>>,
    /// Set by [`Self::shutdown`], read by [`Self::insert_with_id`] under the
    /// same map lock: a child that finishes spawning after the kill sweep is
    /// killed on the way in instead of outliving the store.
    closed: Arc<AtomicBool>,
    /// Serializes process creation with destructive workspace restoration.
    workspace_gate: Arc<tokio::sync::Mutex<()>>,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            owner: uuid::Uuid::new_v4().simple().to_string().into(),
            next_id: Arc::default(),
            jobs: Arc::default(),
            closed: Arc::default(),
            workspace_gate: Arc::default(),
        }
    }
}

impl JobStore {
    pub(super) async fn begin_spawn(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, ToolError> {
        let guard = self.workspace_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err(ToolError::exec_failed("job", "runtime has shut down"));
        }
        Ok(guard)
    }

    /// Hold this guard through the entire restore. A spawn that has not yet
    /// registered its child must finish registration before the idle check.
    pub(crate) async fn idle_workspace(
        &self,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ToolError> {
        let guard = self.workspace_gate.clone().lock_owned().await;
        let jobs = self.jobs.lock().expect("job store lock poisoned");
        for job in jobs.values() {
            let mut job = job.lock().expect("job lock poisoned");
            refresh_job(&mut job);
            let child_alive = job
                .child
                .as_mut()
                .is_some_and(|child| !matches!(child.try_wait(), Ok(Some(_))));
            if job.status == JobStatus::Running || child_alive {
                return Err(ToolError::exec_failed(
                    "checkpoint",
                    "a job is still running — cancel it before restoring the workspace",
                ));
            }
        }
        Ok(guard)
    }

    /// Kill every still-running background child. Called on runtime shutdown so
    /// a cancelled or quit session doesn't orphan long-running processes (dev
    /// servers, watchers) that keep holding ports. `kill_on_drop` is the
    /// backstop; this makes the kill immediate instead of waiting for the store
    /// to drop.
    pub fn shutdown(&self) {
        let guard = self.jobs.lock().expect("job store lock poisoned");
        // Under the map lock, so an `insert_with_id` racing this sweep either
        // lands before it (and is swept) or sees the flag (and kills its own
        // child) — never slips in between with a live process.
        self.closed.store(true, Ordering::SeqCst);
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
                state.close_input();
                state.status = JobStatus::Cancelled;
                state.finished_at = Some(Instant::now());
            }
        }
    }

    /// Allocate the next job id without inserting anything. Split from
    /// [`Self::insert_with_id`] because the spill file paths are named after
    /// the id and must exist before the output buffers (and thus the state)
    /// are constructed.
    pub(super) fn reserve_id(&self) -> String {
        format!(
            "job_{}_{}",
            self.owner,
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    pub(super) fn insert_with_id(&self, id: &str, mut state: JobState) {
        let mut guard = self.jobs.lock().expect("job store lock poisoned");
        // A child that finished spawning after `shutdown` swept the store — a
        // sub-agent abandoned past its cancel grace while still inside
        // `job_start`, say — would otherwise outlive the sweep with only
        // `kill_on_drop` (the direct child, not its process group) behind it.
        // The store is closed: kill it the way the sweep would have.
        if self.closed.load(Ordering::SeqCst)
            && state.status == JobStatus::Running
            && let Some(child) = state.child.as_mut()
        {
            kill_process_tree(child);
            state.job_guard = None;
            state.close_input();
            state.status = JobStatus::Cancelled;
            state.finished_at = Some(Instant::now());
        }
        guard.insert(id.to_string(), Arc::new(Mutex::new(state)));
        evict_finished_jobs(&mut guard);
    }

    pub(super) fn get(&self, id: &str, tool_name: &str) -> Result<Arc<Mutex<JobState>>, ToolError> {
        self.jobs
            .lock()
            .expect("job store lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(tool_name, format!("job_id '{id}' is not owned by this runtime or is no longer retained; jobs cannot be resumed after a runtime restart")))
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

/// Monotonic suffix of a runtime-owned job id, for oldest-first ordering.
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
    pub(super) finished_at: Option<Instant>,
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
    /// Only an explicitly interactive background job retains a writable pipe.
    pub(super) input: Option<Arc<JobInput>>,
    pub(super) input_error: Option<String>,
    /// OS sandbox guard tied to the child (Windows Job Object); dropping it with
    /// the job kills the process tree. `None` on macOS/Linux (confined pre-spawn),
    /// and `None` for a foreground run, whose tool future holds each step's
    /// guard for exactly that step's lifetime instead.
    /// Held purely for its `Drop` — never read.
    #[allow(dead_code)]
    pub(super) job_guard: Option<crate::sandbox::SandboxGuard>,
}

/// One captured view of process state, shared by model and UI projections.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ExecutionSnapshot {
    pub(super) job_id: String,
    pub(super) kind: JobKind,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) status: JobStatus,
    pub(super) exit_code: Option<i32>,
    pub(super) duration_ms: u64,
    pub(super) sandboxed: bool,
    pub(super) network: bool,
    pub(super) stdin_open: bool,
    pub(super) input_error: Option<String>,
    pub(super) stdout: StreamSnapshot,
    pub(super) stderr: StreamSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct StreamSnapshot {
    pub(super) text: String,
    pub(super) total_bytes: usize,
    /// Bytes evicted from the in-memory ring; inline truncation is separate.
    pub(super) omitted_bytes: usize,
    pub(super) inline_truncated: bool,
    pub(super) finished: bool,
    pub(super) read_error: Option<String>,
    /// All stream bytes were captured and remain available in the ring or an uncapped spill.
    pub(super) capture_complete: bool,
    pub(super) spill: Option<SpillInfo>,
}

/// One bounded read of the currently retained bytes. Cursors refer to the
/// original stream, so appending output never changes their meaning.
#[derive(Debug, Clone, Serialize)]
pub(super) struct OutputPage {
    pub(super) stream: super::JobStream,
    pub(super) requested_start_byte: Option<u64>,
    pub(super) start_byte: usize,
    pub(super) end_byte: usize,
    pub(super) retained_start_byte: usize,
    pub(super) unavailable_before_byte: usize,
    pub(super) total_bytes: usize,
    pub(super) next_start_byte: Option<usize>,
    pub(super) has_more: bool,
    pub(super) finished: bool,
    pub(super) lossy_utf8: bool,
    pub(super) read_error: Option<String>,
    pub(super) text: String,
}

impl ExecutionSnapshot {
    pub(super) fn capture(job_id: &str, job: &JobState, max_chars: usize) -> Self {
        Self {
            job_id: job_id.to_string(),
            kind: job.kind,
            command: job.command.clone(),
            cwd: job.cwd.clone(),
            status: job.status,
            exit_code: job.exit_code,
            duration_ms: job
                .finished_at
                .unwrap_or_else(Instant::now)
                .duration_since(job.started_at)
                .as_millis() as u64,
            sandboxed: job.sandboxed,
            network: job.network,
            stdin_open: job.input.as_ref().is_some_and(|input| input.is_open()),
            input_error: job.input_error.clone(),
            stdout: job.stdout.snapshot(max_chars),
            stderr: job.stderr.snapshot(max_chars),
        }
    }
}

impl JobState {
    fn close_input(&mut self) {
        if let Some(input) = self.input.take() {
            input.close();
        }
    }
}

#[derive(Debug)]
pub(super) struct JobInput {
    stream: tokio::sync::Mutex<Option<ChildStdin>>,
    open: AtomicBool,
    closed: tokio_util::sync::CancellationToken,
}

impl JobInput {
    pub(super) fn new(stream: ChildStdin) -> Arc<Self> {
        Arc::new(Self {
            stream: tokio::sync::Mutex::new(Some(stream)),
            open: AtomicBool::new(true),
            closed: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub(super) fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
        self.closed.cancel();
        if let Ok(mut stream) = self.stream.try_lock() {
            stream.take();
        }
    }

    /// A failed write may have delivered a prefix. Close the pipe so a caller
    /// cannot accidentally repeat the rest as though no input reached it.
    pub(super) async fn send(
        &self,
        bytes: &[u8],
        eof: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        let result = {
            let operation = async {
                let mut stream = self.stream.lock().await;
                if !self.is_open() {
                    return Err("stdin is closed".to_string());
                }
                let pipe = stream
                    .as_mut()
                    .ok_or_else(|| "stdin is closed".to_string())?;
                pipe.write_all(bytes).await.map_err(|error| {
                    format!("stdin write failed; input may have been partially delivered: {error}")
                })?;
                if eof {
                    self.open.store(false, Ordering::SeqCst);
                    stream.take();
                }
                Ok(())
            };
            tokio::pin!(operation);
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err("stdin write cancelled; input may have been partially delivered; inspect the job before retrying".to_string()),
                () = self.closed.cancelled() => Err("stdin is closed; input may have been partially delivered".to_string()),
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => Err("stdin write timed out; input may have been partially delivered; inspect the job before retrying".to_string()),
                result = &mut operation => result,
            }
        };
        if result.is_err() {
            self.close();
        }
        result
    }
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

    pub(super) fn push(&self, bytes: &[u8]) {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .push(bytes);
    }

    pub(super) fn text(&self) -> String {
        self.0.lock().expect("output buffer lock poisoned").text()
    }

    #[cfg(test)]
    fn total_len(&self) -> usize {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .total_len
    }

    #[cfg(test)]
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

    /// Stream over: release the spill file handle. The file stays on disk —
    /// transcript references must outlive the job record — and whether it was
    /// worth writing is decided later, by [`Self::discard_unreported_spill`].
    ///
    /// Call it only when no further writer remains for this buffer, because
    /// [`Spill::finish`] is one-way. Which side that is depends on the caller
    /// and is stated once, on [`SpillOwner`]: a background job's reader, or —
    /// for a foreground `shell` call, whose steps share one buffer pair — the
    /// run itself, after its last step. Idempotent, and safe against a reader
    /// task aborted mid-flight: see `finished` in [`Spill`], which stops a
    /// still-winding-down push from re-opening the file.
    pub(super) fn finish_spill(&self) {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        if let Some(spill) = ring.spill.as_mut() {
            spill.finish();
        }
    }

    pub(super) fn finish_stream(&self, error: Option<String>) {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        ring.finished = true;
        if let Some(error) = error {
            ring.read_error.get_or_insert(error);
        }
        if let Some(spill) = ring.spill.as_mut() {
            spill.finish();
        }
    }

    /// Record incomplete capture without ending a run that has later writers.
    pub(super) fn record_read_error(&self, error: String) {
        self.0
            .lock()
            .expect("output buffer lock poisoned")
            .read_error
            .get_or_insert(error);
    }

    pub(super) fn page(
        &self,
        stream: super::JobStream,
        requested: Option<u64>,
        max_chars: usize,
    ) -> Result<OutputPage, ToolError> {
        let ring = self.0.lock().expect("output buffer lock poisoned");
        let bytes: Vec<_> = ring.bytes.iter().copied().collect();
        let retained_start = ring.omitted_len();
        // Eviction can cut through a UTF-8 scalar. Its remaining continuation
        // bytes cannot be reconstructed, and belong to the unavailable prefix.
        let partial_prefix = if retained_start > 0 {
            let continuation_bytes = bytes
                .iter()
                .take(4)
                .take_while(|byte| **byte & 0xc0 == 0x80)
                .count();
            // A cut scalar can leave at most three continuation bytes. A
            // longer run is invalid data, not one truncated character.
            if continuation_bytes <= 3 {
                continuation_bytes
            } else {
                0
            }
        } else {
            0
        };
        let available_start = retained_start + partial_prefix;
        let requested_offset = requested
            .map(usize::try_from)
            .transpose()
            .map_err(|_| invalid("job", "start_byte exceeds the supported stream offset"))?
            .unwrap_or(available_start);
        if requested_offset > ring.total_len {
            return Err(invalid(
                "job",
                "start_byte exceeds the captured stream length",
            ));
        }
        let start = requested_offset.max(available_start);
        let offset = start - retained_start;
        if inside_utf8_scalar(&bytes, offset) {
            return Err(invalid(
                "job",
                "start_byte is inside a UTF-8 character; use a returned cursor",
            ));
        }
        let page = read_page_text(
            &bytes[offset..],
            ring.finished,
            max_chars.clamp(1, INLINE_STREAM_CHARS),
        );
        let end = start + page.consumed;
        Ok(OutputPage {
            stream,
            requested_start_byte: requested,
            start_byte: start,
            end_byte: end,
            retained_start_byte: retained_start,
            unavailable_before_byte: available_start,
            total_bytes: ring.total_len,
            next_start_byte: (page.has_more || !ring.finished).then_some(end),
            has_more: page.has_more,
            finished: ring.finished,
            lossy_utf8: page.lossy,
            read_error: ring.read_error.clone(),
            text: page.text,
        })
    }

    fn snapshot(&self, max_chars: usize) -> StreamSnapshot {
        let mut ring = self.0.lock().expect("output buffer lock poisoned");
        let full = ring.text();
        let count = full.chars().count();
        let text = if count > max_chars {
            full.chars().skip(count - max_chars).collect()
        } else {
            full
        };
        let omitted_bytes = ring.omitted_len();
        let inline_truncated = omitted_bytes > 0 || count > max_chars;
        let spill = if inline_truncated {
            ring.spill.as_mut().and_then(|spill| {
                let info = spill.info();
                if info.is_some() {
                    spill.reported = true;
                }
                info
            })
        } else {
            None
        };
        StreamSnapshot {
            text,
            total_bytes: ring.total_len,
            omitted_bytes,
            inline_truncated,
            finished: ring.finished,
            read_error: ring.read_error.clone(),
            capture_complete: ring.finished
                && ring.read_error.is_none()
                && (omitted_bytes == 0 || spill.as_ref().is_some_and(|info| !info.capped)),
            spill,
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

fn inside_utf8_scalar(bytes: &[u8], offset: usize) -> bool {
    (1..=3.min(offset)).any(|back| {
        let start = offset - back;
        let width = match bytes[start] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return false,
        };
        width > back
            && start + width <= bytes.len()
            && std::str::from_utf8(&bytes[start..start + width]).is_ok()
    })
}

struct PageText {
    text: String,
    consumed: usize,
    chars: usize,
    json_bytes: usize,
    has_more: bool,
    lossy: bool,
}

impl PageText {
    fn push(&mut self, character: char, source_bytes: usize, max_chars: usize) -> bool {
        let encoded = match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\0'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        // Bound both model text and the JSON details. This leaves room for
        // execution metadata without duplicating the two tail windows.
        if self.chars >= max_chars || self.json_bytes + encoded > 8 * 1024 {
            self.has_more = true;
            return false;
        }
        self.text.push(character);
        self.consumed += source_bytes;
        self.chars += 1;
        self.json_bytes += encoded;
        true
    }
}

fn read_page_text(bytes: &[u8], finished: bool, max_chars: usize) -> PageText {
    let mut page = PageText {
        text: String::new(),
        consumed: 0,
        chars: 0,
        json_bytes: 2,
        has_more: false,
        lossy: false,
    };
    'page: while page.consumed < bytes.len() {
        let remaining = &bytes[page.consumed..];
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(text) => (text, None),
            Err(error) => (
                std::str::from_utf8(&remaining[..error.valid_up_to()]).expect("valid UTF-8 prefix"),
                Some(error.error_len()),
            ),
        };
        for character in valid.chars() {
            if !page.push(character, character.len_utf8(), max_chars) {
                break 'page;
            }
        }
        let invalid_bytes = match invalid {
            None => break,
            Some(Some(count)) => count,
            Some(None) if finished => bytes.len() - page.consumed,
            Some(None) => break, // A running stream may append the remaining UTF-8 bytes.
        };
        if !page.push('\u{fffd}', invalid_bytes, max_chars) {
            break;
        }
        page.lossy = true;
    }
    page
}

/// Facts about a written spill file, for the model-facing note and details.
#[derive(Debug, Clone, Serialize)]
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
    finished: bool,
    read_error: Option<String>,
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
            finished: false,
            read_error: None,
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
/// as group leaders, see `spawn_confined`), so grandchildren the shell spawned
/// into that group don't outlive the job. A descendant that moved itself into
/// a new session (`setsid`, a double-forking daemon) is outside the group and
/// is not reached: it stays inside the sandbox, but not inside the job's
/// lifetime — a `network=true` job's detached child keeps its egress after a
/// cancel. Reaching it would take a cgroup (Linux only); this is the documented
/// residual. On non-Unix `start_kill` only reaps the direct child; killing the
/// tree there relies on dropping the job's Windows Job Object guard (see the
/// `job_guard = None` at each kill site), which does contain a detached child.
pub(super) fn kill_process_tree(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.start_kill();
}

/// Retain the leader PID before `try_wait`/`wait` reaps it: Tokio clears the
/// child's id on exit, while its process group can still contain descendants.
pub(super) fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pid;
}

pub(super) type ChunkFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Who releases a buffer's spill handle when a reader reaches end-of-stream.
///
/// The question exists because the two callers own their stream differently,
/// and answering it the same way for both is what made a `&&` chain lose
/// output. [`Spill::finish`] is ONE-WAY — `offer` short-circuits on `finished`
/// forever after — so whoever calls it must be the last writer of that buffer.
///
/// * A background `job action=start` is one process with one reader per
///   stream, and nothing comes back to the buffers afterwards: the reader IS
///   the last writer, so it finishes ([`Self::Reader`]).
/// * A foreground `shell` call is a SEQUENCE. Every step spawns its own reader
///   but they all push into the one buffer pair the run created, so the first
///   step's end-of-stream is not the run's. A reader that finished there
///   stopped spilling for every step after it — silently, on the ordinary
///   path, not just when a lingering grandchild forced an abort:
///   `cargo build && cargo test` is two default-trusted steps, so a quiet
///   build left `cargo test`'s log with no spill file at all, and a loud one
///   left a file holding only the build while the truncation note called it
///   "the complete stream". There the RUN owns the handle ([`Self::Caller`])
///   and releases it once, after the last step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpillOwner {
    /// This reader is the buffer's only writer: finish at end-of-stream.
    Reader,
    /// Several readers share this buffer; the caller finishes it when the
    /// whole run is over.
    Caller,
}

/// Drain one child pipe into the ring buffer, optionally forwarding each
/// chunk (live streaming for foreground shells; background jobs pass `None`
/// because their parent turn has already ended). Returns the reader task so
/// the foreground shell can await it (EOF) before reading the buffer.
///
/// `spill_owner` says whether reaching end-of-stream here also releases the
/// buffer's spill handle — see [`SpillOwner`], which is not a preference.
pub(super) fn spawn_buffer_reader<R>(
    mut pipe: R,
    buffer: SharedBuffer,
    on_chunk: Option<ChunkFn>,
    spill_owner: SpillOwner,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        let mut error = None;
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) => break,
                Ok(bytes) => {
                    buffer.push(&chunk[..bytes]);
                    if let Some(on_chunk) = &on_chunk {
                        on_chunk(&chunk[..bytes]);
                    }
                }
                Err(cause) => {
                    error = Some(cause.to_string());
                    break;
                }
            }
        }
        // Stream over: release the spill file handle (data is already
        // written unbuffered). The file stays on disk for later reads —
        // unless it turned out to be an orphan nothing will ever name.
        if spill_owner == SpillOwner::Reader {
            buffer.finish_stream(error);
        } else if let Some(error) = error {
            buffer.record_read_error(error);
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
    let pid = child.id();
    if let Ok(Some(status)) = child.try_wait() {
        // A background command owns its group only while its leader runs.
        // Otherwise `sh -c 'writer & exit'` would leave a writer behind after
        // this job reports completion and the workspace restore gate opens.
        kill_process_group(pid);
        job.job_guard = None;
        job.close_input();
        job.exit_code = status.code();
        job.status = if status.success() {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        job.finished_at = Some(Instant::now());
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
        job.close_input();
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
    job.finished_at = Some(Instant::now());
    Ok(())
}

#[cfg(test)]
mod tests;
