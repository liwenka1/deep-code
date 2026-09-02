//! Model-facing rendering of shell/job output: the plain-text result body, the
//! truncation and sandbox-denial notes, the `job` tool's status/tail snapshot
//! and details JSON, and the byte/duration formatters.
//!
//! Split from the process/buffer machinery in the parent module — this is
//! presentation, not job bookkeeping, and it only reads `JobState`/`SharedBuffer`
//! accessors. `use super::*` inherits the parent's types, imports and constants.

use super::*;

/// Model-facing plain-text output for a finished foreground shell command.
pub(in crate::shell_tools) fn shell_text_output(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
) -> String {
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
pub(in crate::shell_tools) fn job_text_snapshot(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
) -> String {
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
pub(in crate::shell_tools) fn job_details(job_id: &str, job: &JobState) -> Value {
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

fn tail_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    value.chars().skip(count - max_chars).collect()
}
