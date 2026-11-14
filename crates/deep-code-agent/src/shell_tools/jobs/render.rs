//! Model and UI projections of a captured execution state.

use super::*;
use crate::tool::ToolOutput;
#[cfg(test)]
use serde_json::Value;

pub(in crate::shell_tools) fn job_output(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
) -> ToolOutput {
    let snapshot = ExecutionSnapshot::capture(job_id, job, max_chars.min(INLINE_STREAM_CHARS));
    let text = render_snapshot(&snapshot, denial_note(job));
    let details = serde_json::to_value(&snapshot).expect("execution snapshots are serializable");
    if snapshot.stdout.finished && !snapshot.stdout.inline_truncated {
        job.stdout.discard_unreported_spill();
    }
    if snapshot.stderr.finished && !snapshot.stderr.inline_truncated {
        job.stderr.discard_unreported_spill();
    }
    ToolOutput::text(text).with_details(details)
}

pub(in crate::shell_tools) fn job_page_output(
    job_id: &str,
    job: &JobState,
    stream: super::super::JobStream,
    start_byte: Option<u64>,
    max_chars: usize,
) -> Result<ToolOutput, ToolError> {
    let buffer = match stream {
        super::super::JobStream::Stdout => &job.stdout,
        super::super::JobStream::Stderr => &job.stderr,
    };
    let page = buffer.page(stream, start_byte, max_chars)?;
    // The page is the payload; two additional tail windows would duplicate it
    // and overrun the shared model/details budgets.
    let snapshot = ExecutionSnapshot::capture(job_id, job, 0);
    let mut details = serde_json::to_value(&snapshot).expect("execution snapshots serialize");
    details["output_page"] = serde_json::to_value(&page).expect("output pages serialize");
    let mut text = format!(
        "{job_id} ({:?}) — {} · {} · cmd: {}\n[{} retained output bytes {}..{}; retained range {}..{}; finished={}; has_more={}]\n",
        snapshot.kind,
        snapshot.status.as_str(),
        format_elapsed(snapshot.duration_ms),
        crate::text_util::truncate_chars(&snapshot.command, 512),
        stream.as_str(),
        page.start_byte,
        page.end_byte,
        page.retained_start_byte,
        page.total_bytes,
        page.finished,
        page.has_more,
    );
    if page.unavailable_before_byte > 0 {
        text.push_str(&format!(
            "[bytes before {} are unavailable as text from the ring; use a spill file if one was reported]\n",
            page.unavailable_before_byte,
        ));
    }
    text.push_str(&page.text);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if let Some(error) = &page.read_error {
        text.push_str(&format!(
            "[{} capture failed: {error}; output is incomplete]\n",
            stream.as_str()
        ));
    }
    if page.lossy_utf8 {
        text.push_str("[invalid UTF-8 bytes are shown as replacement characters; byte cursors still refer to the original stream]\n");
    }
    match page.next_start_byte {
        Some(next) => {
            if !page.has_more {
                text.push_str(
                    "[no more buffered text now; wait for more output before resuming]\n",
                );
            }
            text.push_str(&format!(
                "[resume: job action=tail job_id={job_id} stream={} start_byte={next} max_chars={}]",
                stream.as_str(), max_chars.min(INLINE_STREAM_CHARS),
            ));
        }
        None => text.push_str("[end of retained output]"),
    }
    Ok(ToolOutput::text(text).with_details(details))
}

#[cfg(test)]
pub(in crate::shell_tools) fn shell_text_output(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
) -> String {
    job_output(job_id, job, max_chars).content.text()
}

#[cfg(test)]
pub(in crate::shell_tools) fn job_text_snapshot(
    job_id: &str,
    job: &JobState,
    max_chars: usize,
) -> String {
    shell_text_output(job_id, job, max_chars)
}

#[cfg(test)]
pub(in crate::shell_tools) fn job_details(job_id: &str, job: &JobState) -> Value {
    job_output(job_id, job, INLINE_STREAM_CHARS)
        .details
        .expect("execution output carries details")
}

fn render_snapshot(snapshot: &ExecutionSnapshot, denial: Option<&str>) -> String {
    let command = crate::text_util::truncate_chars(&snapshot.command, 512);
    let mut out = format!(
        "{} ({:?}) — {} · {} · cmd: {}\n[process status: {}; exit code: {}; stdin: {}]\n",
        snapshot.job_id,
        snapshot.kind,
        snapshot.status.as_str(),
        format_elapsed(snapshot.duration_ms),
        command,
        snapshot.status.as_str(),
        snapshot
            .exit_code
            .map_or_else(|| "none".to_string(), |code| code.to_string()),
        if snapshot.stdin_open {
            "open"
        } else {
            "closed"
        },
    );
    if snapshot.stdout.text.is_empty() && snapshot.stderr.text.is_empty() {
        out.push_str("(no output)\n");
    }
    for (name, stream) in [("stdout", &snapshot.stdout), ("stderr", &snapshot.stderr)] {
        if !stream.text.is_empty() {
            out.push_str(&format!("[{name}]\n{}", stream.text));
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    match snapshot.status {
        JobStatus::Running => out
            .push_str("[still running; this tool call returning does not mean the process exited]"),
        JobStatus::TimedOut => {
            out.push_str("[timed out — process killed; previous effects are not rolled back]")
        }
        JobStatus::Cancelled => {
            out.push_str("[cancelled — process killed; previous effects are not rolled back]")
        }
        _ => out.push_str(&format!(
            "[exit {} · {}]",
            snapshot
                .exit_code
                .map_or_else(|| "?".to_string(), |code| code.to_string()),
            format_elapsed(snapshot.duration_ms)
        )),
    }
    if let Some(error) = &snapshot.input_error {
        out.push_str(&format!("\n[stdin: {error}]"));
    }
    for (name, stream) in [("stdout", &snapshot.stdout), ("stderr", &snapshot.stderr)] {
        if stream.inline_truncated {
            out.push_str(&format!("\n[{name} truncated"));
            match &stream.spill {
                Some(spill) if spill.capped => out.push_str(&format!(" — first {} saved: '{}' (output exceeded the per-stream cap, so the file holds the head only); read that file for the parts not shown]", format_bytes(spill.bytes), spill.path.display())),
                Some(spill) => out.push_str(&format!(" — {} stream saved: '{}' ({}); read that file for the parts not shown]", if stream.capture_complete { "complete" } else { "captured" }, spill.path.display(), format_bytes(spill.bytes))),
                None => out.push_str(&format!(" — page retained output: job action=tail job_id={} stream={name} start_byte={} max_chars={}; bytes before {} are unavailable from the ring]", snapshot.job_id, stream.omitted_bytes, INLINE_STREAM_CHARS, stream.omitted_bytes)),
            }
        }
        if let Some(error) = &stream.read_error {
            out.push_str(&format!(
                "\n[{name} capture failed: {error}; output is incomplete]"
            ));
        } else if !stream.finished && snapshot.status != JobStatus::Running {
            out.push_str(&format!(
                "\n[{name} capture has not reached EOF; output may still be draining]"
            ));
        }
    }
    if let Some(note) = denial {
        out.push('\n');
        out.push_str(note);
    }
    out
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
    if job.status != JobStatus::Failed {
        return None;
    }
    let stderr = job.stderr.text();
    if !job.sandboxed {
        // No fence produced this failure, so only the class that is not about a
        // fence applies. An unsandboxed `Operation not permitted` stays silent —
        // that is an ordinary permission problem, not the granted-roots fence —
        // while an authentication failure is the same diagnosis either way, and
        // `[sandbox] mode = "off"` is the configuration most likely to run into
        // it.
        return crate::sandbox::credential_denial_signature(job.exit_code, &stderr)
            .then_some(crate::sandbox::CREDENTIAL_DENIAL_NOTE);
    }
    if !job.network && crate::sandbox::network_denial_signature(job.exit_code, &stderr) {
        return Some(crate::sandbox::NETWORK_DENIAL_NOTE);
    }
    // Credentials come before the write fence: both notes share `Permission
    // denied`, and a push that failed on auth would otherwise be handed the
    // granted-roots advice, which is exactly wrong. Ungated by `job.network` —
    // this failure happens after connecting, so it is equally possible with and
    // without the grant.
    if crate::sandbox::credential_denial_signature(job.exit_code, &stderr) {
        return Some(crate::sandbox::CREDENTIAL_DENIAL_NOTE);
    }
    crate::sandbox::write_denial_signature(job.exit_code, &stderr)
        .then_some(crate::sandbox::WRITE_DENIAL_NOTE)
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
