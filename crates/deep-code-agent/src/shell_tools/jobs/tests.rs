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
        job.stdout.text().chars().count() < budget && job.stderr.text().chars().count() < budget,
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
    assert!(shell_text_output("job_1", &denied, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE));
    assert!(job_text_snapshot("job_1", &denied, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE));

    // Same failure without the sandbox: a plain permission problem — the
    // granted-roots fence was not involved, so no note.
    let bare = finished_job(JobStatus::Failed, false, "sh: Operation not permitted");
    assert!(!shell_text_output("job_2", &bare, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE));

    // Sandboxed failure with an unrelated stderr: no note.
    let unrelated = finished_job(JobStatus::Failed, true, "error: expected `;`");
    assert!(
        !shell_text_output("job_3", &unrelated, 4096).contains(crate::sandbox::WRITE_DENIAL_NOTE)
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

/// `as_str` hand-spells what `#[serde(rename_all = "snake_case")]` derives;
/// the two are the same word on the wire and in the model-facing text, so pin
/// them together rather than trusting the duplication to stay in step.
#[test]
fn job_status_as_str_is_its_serde_spelling() {
    for status in [
        JobStatus::Running,
        JobStatus::Completed,
        JobStatus::Failed,
        JobStatus::TimedOut,
        JobStatus::Cancelled,
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::Value::String(status.as_str().to_string()),
            "{status:?}"
        );
    }
}
