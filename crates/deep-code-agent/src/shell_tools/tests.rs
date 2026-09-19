use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::tool::{ApprovalDecision, ToolCall, ToolResult, ToolResultStatus, ToolRunOutcome};

/// Platform-agnostic 5-second sleep command. `sleep 5` only exists in
/// Unix `sh`; Windows `cmd` needs `ping -n 6 127.0.0.1 > nul`.
fn sleep_5() -> &'static str {
    if cfg!(windows) {
        "ping -n 6 127.0.0.1 > nul"
    } else {
        "sleep 5"
    }
}

/// Platform-agnostic 2-second background-able command that echoes "hello".
/// Unix: `printf hello && sleep 2`; Windows: `echo hello & ping -n 3 127.0.0.1 > nul`.
fn echo_and_sleep_2() -> &'static str {
    if cfg!(windows) {
        "echo hello & ping -n 3 127.0.0.1 > nul"
    } else {
        "printf hello && sleep 2"
    }
}

fn registry(root: &std::path::Path) -> ToolRegistry {
    ShellTools::new(root)
        .unwrap()
        .with_sandbox(SandboxManager::new().force_sandbox(Some(false)))
        .into_registry()
}

async fn approved(root: &std::path::Path, name: &str, arguments: Value) -> ToolResult {
    let call = ToolCall::new("call_1", name, arguments);
    let ToolRunOutcome::Result { result } = registry(root)
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    result
}

fn details(result: &ToolResult) -> &Value {
    result
        .details
        .as_ref()
        .expect("shell results carry details")
}

/// Unix-only because the command it uses is: `echo` is a trusted prefix only
/// where it names a real program. A trusted command runs as argv with no shell
/// (`RunAuthority::Parse`), and on Windows `echo` is a `cmd` builtin that
/// resolves to no file — so it is not in the default trust list there (see
/// `ExecPolicy::default`), and `echo hello` takes the approval path instead.
/// The Windows twin below covers the same result/details plumbing through it.
#[cfg(unix)]
#[tokio::test]
async fn shell_trusted_command_runs_without_approval() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    // `echo ` is a trusted prefix: policy allows it without prompting now that
    // the spec-level approval flag no longer overrides the trust table.
    let call = ToolCall::new("call_1", "shell", json!({"command": "echo hello"}));

    let ToolRunOutcome::Result { result } = registry.run_tool_call(call, None).await.unwrap()
    else {
        panic!("expected trusted command to run without approval");
    };
    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.content.contains("hello"));
    assert!(result.content.contains("[exit 0"));
    let info = details(&result);
    assert_eq!(info["status"], "completed");
    assert_eq!(info["exit_code"], 0);
    assert_eq!(info["kind"], "foreground");

    // The foreground run is visible to the job tool afterwards.
    let job_id = info["job_id"].as_str().unwrap();
    let status = ToolCall::new(
        "call_2",
        "job",
        json!({"action": "status", "job_id": job_id}),
    );
    let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).await.unwrap()
    else {
        panic!("expected result");
    };
    assert_eq!(details(&result)["kind"], "foreground");
}

#[tokio::test]
async fn shell_untrusted_command_requires_approval() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "shell", json!({"command": "python --version"}));

    assert!(matches!(
        registry.run_tool_call(call, None).await.unwrap(),
        ToolRunOutcome::ApprovalRequired { .. }
    ));
}

#[tokio::test]
async fn shell_reports_failure() {
    let tmp = tempdir().unwrap();
    let result = approved(tmp.path(), "shell", json!({"command": "exit 7"})).await;
    assert!(result.content.contains("[exit 7"));
    let info = details(&result);
    assert_eq!(info["status"], "failed");
    assert_eq!(info["exit_code"], 7);
}

#[tokio::test]
async fn shell_times_out_and_kills_the_child() {
    let tmp = tempdir().unwrap();
    let started = Instant::now();
    let result = approved(
        tmp.path(),
        "shell",
        json!({"command": sleep_5(), "timeout_secs": 1}),
    )
    .await;
    assert!(result.content.contains("timed out"));
    assert_eq!(details(&result)["status"], "timed_out");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "timeout must not wait for the child's natural exit"
    );
}

/// End-to-end spill: an output past the inline window lands byte-complete in
/// a file under the workspace's `.deep-code/spill/`, the result text names
/// that file, and the structured details carry the path. The HEAD assertion
/// is the point of the feature — before spill, everything before the tail
/// window was physically unrecoverable.
#[cfg(unix)]
#[tokio::test]
async fn shell_overflow_spills_full_stream_and_result_names_the_file() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    // ~28 KB: past the 20k inline window, within the 128 KiB ring — the
    // range that used to truncate silently.
    let call = ToolCall::new("call_1", "shell", json!({"command": "seq 1 6000"}));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(
        result.content.contains("complete stream saved"),
        "result must point at the spill file: {}",
        result.content
    );

    let spill_path = details(&result)["stdout_spill_path"]
        .as_str()
        .expect("details carry the spill path")
        .to_string();
    assert!(spill_path.contains(".deep-code"));
    let saved = std::fs::read_to_string(&spill_path).unwrap();
    assert!(
        saved.starts_with("1\n2\n"),
        "spill must preserve the head the inline tail cut off"
    );
    assert!(saved.ends_with("6000\n"), "…and the very end of the stream");

    // The spilled stream stays readable after the job record is gone: the
    // reader task closed its handle at EOF, the file itself persists.
    assert!(std::path::Path::new(&spill_path).exists());
}

/// `drain_readers` reports whether it had to abort, and finishes no spill of
/// its own.
///
/// The spill handles belong to the *run*, not to one step of it. A shell call
/// is a sequence now and every step shares one pair of buffers, while `finish`
/// is one-way — a late `push` cannot re-open a finished spill. Releasing them
/// inside the per-step drain therefore stopped spilling for every step after
/// the first one whose grandchild held the pipe past the cap: a `&&` chain lost
/// the full output of all the rest while the truncation hint still named a file
/// missing it, silently, on the non-exceptional path. The buffers are simply
/// not in scope in `drain_readers` any more, so the signature is the fence;
/// what is left to pin is the answer the caller acts on.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drain_readers_reports_whether_it_aborted() {
    // A reader that never reaches end-of-stream — the lingering-grandchild
    // case the 500 ms cap exists for. Virtual time, so the cap costs nothing.
    let stuck = tokio::spawn(std::future::pending::<()>());
    assert!(
        drain_readers(Some(stuck), None).await,
        "an aborted reader leaves the spill handles owed to the caller"
    );
    // Both readers already at end-of-stream: they finished their own spill.
    let done = tokio::spawn(async {});
    assert!(
        !drain_readers(Some(done), None).await,
        "a clean drain owes the caller nothing"
    );
}

/// Small outputs must stay diskless: no spill file, no spill directory.
#[tokio::test]
async fn small_output_leaves_no_spill_dir_behind() {
    let tmp = tempdir().unwrap();
    let result = approved(tmp.path(), "shell", json!({"command": "echo hi"})).await;
    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(details(&result)["stdout_spill_path"].is_null());
    assert!(!tmp.path().join(".deep-code/spill").exists());
}

/// `justification` is a declared schema field: a call carrying it must parse
/// and run (regression guard — `deny_unknown_fields` rejected it before the
/// field existed, which would have made the tool description a trap).
#[tokio::test]
async fn shell_accepts_a_justification_argument() {
    let tmp = tempdir().unwrap();
    let result = approved(
        tmp.path(),
        "shell",
        json!({"command": "echo ok", "justification": "prove the field parses"}),
    )
    .await;
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );
    assert!(result.content.contains("ok"));
}

/// Timeout must kill the whole process tree (the child is spawned as its own
/// process group), not just the immediate shell — otherwise grandchildren
/// like spawned servers keep running and holding ports.
#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_kills_grandchildren_too() {
    let tmp = tempdir().unwrap();
    let result = approved(
        tmp.path(),
        "shell",
        json!({"command": "sleep 30 & echo started:$!; wait", "timeout_secs": 1}),
    )
    .await;
    assert_eq!(details(&result)["status"], "timed_out");

    let pid: i32 = result
        .content
        .lines()
        .find_map(|line| line.trim().strip_prefix("started:"))
        .expect("grandchild pid echoed")
        .trim()
        .parse()
        .expect("pid parses");
    // The grandchild may linger as a zombie for a beat until init reaps it;
    // poll until the pid is gone (ESRCH) rather than asserting instantly.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "grandchild sleep (pid {pid}) survived the process-group kill"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn shell_streams_chunks_via_on_update() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "shell", json!({"command": "echo hello"}));

    let updates: Arc<Mutex<Vec<crate::tool::ToolUpdate>>> = Arc::default();
    let sink = Arc::clone(&updates);
    let cx = crate::tool::ToolCx::new().with_update_fn(Arc::new(move |update| {
        sink.lock().unwrap().push(update);
    }));
    let plan = registry.evaluate_tool(&call);
    let outcome = registry
        .run_tool_call_with_plan(&call, Some(ApprovalDecision::Approved), plan, cx)
        .await
        .unwrap();
    let ToolRunOutcome::Result { result } = outcome else {
        panic!("expected result");
    };
    assert_eq!(result.status, ToolResultStatus::Success);

    let streamed = updates
        .lock()
        .unwrap()
        .iter()
        .map(|update| update.text.clone())
        .collect::<String>();
    assert!(
        streamed.contains("hello"),
        "expected live-streamed stdout, got: {streamed:?}"
    );
}

#[tokio::test]
async fn shell_kill_on_cancellation() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "shell", json!({"command": sleep_5()}));

    let cancel = CancellationToken::new();
    let cx = crate::tool::ToolCx::new().with_cancel(cancel.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let plan = registry.evaluate_tool(&call);
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call_with_plan(&call, Some(ApprovalDecision::Approved), plan, cx)
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert!(result.content.contains("cancelled"));
    assert_eq!(details(&result)["status"], "cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "cancellation must kill the child promptly"
    );
}

#[tokio::test]
async fn shell_rejects_cwd_escape() {
    let tmp = tempdir().unwrap();
    let call = ToolCall::new(
        "call_1",
        "shell",
        json!({"command": "pwd", "cwd": "../outside"}),
    );
    assert!(matches!(
        registry(tmp.path())
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .await,
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[tokio::test]
async fn job_start_status_tail_and_cancel_work() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    // A chained command (`printf ... && sleep`) is not auto-trusted — the
    // trusted `printf` prefix no longer blesses the `sleep` tail — so start is
    // run with an explicit approval.
    let call = ToolCall::new(
        "call_1",
        "job",
        json!({"action": "start", "command": echo_and_sleep_2()}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert!(result.content.contains("started job_"));
    let job_id = details(&result)["job_id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(80)).await;
    let tail = ToolCall::new("call_2", "job", json!({"action": "tail", "job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(tail, None).await.unwrap()
    else {
        panic!("expected result");
    };
    assert!(result.content.contains("hello"));
    assert_eq!(details(&result)["status"], "running");

    let cancel = ToolCall::new(
        "call_3",
        "job",
        json!({"action": "cancel", "job_id": job_id}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(cancel, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert_eq!(details(&result)["status"], "cancelled");
}

#[tokio::test]
async fn job_store_shutdown_kills_running_background_jobs() {
    let tmp = tempdir().unwrap();
    let shell = ShellTools::new(tmp.path())
        .unwrap()
        .with_sandbox(SandboxManager::new().force_sandbox(Some(false)));
    let jobs = shell.job_store();
    let registry = shell.into_registry();

    let call = ToolCall::new(
        "call_1",
        "job",
        json!({"action": "start", "command": echo_and_sleep_2()}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert_eq!(details(&result)["status"], "running");
    let job_id = details(&result)["job_id"].as_str().unwrap().to_string();

    // Runtime teardown must terminate the still-running child, not orphan it.
    jobs.shutdown();

    let status = ToolCall::new(
        "call_2",
        "job",
        json!({"action": "status", "job_id": job_id}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(status, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    assert_eq!(details(&result)["status"], "cancelled");
}

#[tokio::test]
async fn job_actions_validate_required_params() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());

    let start = ToolCall::new("c1", "job", json!({"action": "start"}));
    assert!(matches!(
        registry
            .run_tool_call(start, Some(ApprovalDecision::Approved))
            .await,
        Err(ToolError::InvalidArguments { .. })
    ));

    let status = ToolCall::new("c2", "job", json!({"action": "status"}));
    assert!(matches!(
        registry.run_tool_call(status, None).await,
        Err(ToolError::InvalidArguments { .. })
    ));
}

/// `scrub_secret_env` must strip provider/runtime secrets so a spawned shell
/// cannot read them from its own environment. The var is set explicitly on the
/// command (no global env mutation → no cross-test races); the child must see
/// nothing after scrubbing.
#[cfg(unix)]
#[tokio::test]
async fn scrub_secret_env_hides_key_from_child() {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(r#"printf %s "$DEEPSEEK_API_KEY""#)
        .env(crate::config::DEEPSEEK_API_KEY_ENV, "super-secret-sentinel");
    scrub_secret_env(&mut cmd);
    let output = cmd.output().await.expect("spawn sh");
    assert!(
        output.stdout.is_empty(),
        "child leaked the key: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The same scrub drops linker/shell code-injection vectors, so an inherited
/// `LD_PRELOAD`/`DYLD_INSERT_LIBRARIES`/`BASH_ENV`/`ENV` cannot run code the
/// approved command line never named. Asserts each is absent from the child's
/// environment (env absence, not shell behavior, so no bash-version dependency).
#[cfg(unix)]
#[tokio::test]
async fn scrub_secret_env_strips_injection_vectors() {
    for var in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "ENV",
    ] {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!(r#"printf %s "${var}""#))
            .env(var, "/tmp/evil-sentinel");
        scrub_secret_env(&mut cmd);
        let output = cmd.output().await.expect("spawn sh");
        assert!(
            output.stdout.is_empty(),
            "{var} survived scrubbing: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

/// The model must never be told it is confined when it is not. Both tool
/// descriptions are picked from the host's real capability, so this asserts the
/// two stay in agreement — on Windows (Job Object: no fs/network confinement)
/// the "runs sandboxed without network" wording would be a lie, and a model that
/// believes it is offline will not declare the network it silently already has.
///
/// Three states, not two: a host that confines writes apart from a right its
/// kernel cannot express must say so here as well as in the approval panel.
/// The model is the surface that actually issues the write, so rounding a gap
/// up to "confined" here is the costliest place to do it.
///
/// A partial host is checked gap-by-gap rather than against one fixed phrase.
/// Two reasons: the caveats differ (the `truncate` gap denies part of the write
/// boundary, the device-`ioctl` gap does not), and the `Full` arm is unreachable
/// on any Linux below 6.10 — so a single shared phrase let the promises that
/// only `Full` asserted go uncovered on exactly the hosts that ship.
#[test]
fn tool_descriptions_match_actual_sandbox_capability() {
    use crate::sandbox::EnforcementGap;
    use crate::tool::Tool;

    const ALL_GAPS: &[EnforcementGap] = &[
        EnforcementGap::LandlockTruncate,
        EnforcementGap::LandlockIoctlDev,
    ];

    let enforcement = crate::sandbox::sandbox_enforcement();
    let shell = ShellTool::new(
        WorkspacePolicy::new(std::path::Path::new(".")).unwrap(),
        JobStore::default(),
        SandboxManager::new(),
        std::path::PathBuf::from(".deep-code/spill/test"),
    );
    let job = JobTool::new(
        WorkspacePolicy::new(std::path::Path::new(".")).unwrap(),
        JobStore::default(),
        SandboxManager::new(),
        std::path::PathBuf::from(".deep-code/spill/test"),
    );

    for description in [shell.description(), job.description()] {
        // Asserted for Full AND Partial: a gap qualifies the boundary, it does
        // not withdraw it, so the confinement promise has to survive either way.
        // Only `None` may drop it.
        if enforcement.is_enforced() {
            assert!(
                description.contains("sandboxed without network"),
                "a confining host must advertise confinement: {description}"
            );
            assert!(
                description.contains("granted roots"),
                "a confining host must name the write boundary: {description}"
            );
            assert!(
                description.contains("request_write_root"),
                "a described boundary must teach its grant channel: {description}"
            );
            assert!(!description.contains("NO OS sandbox"));
        }
        // Exactly this host's gaps are named — no more, no less. "No more" is
        // the half that matters: it is what stops the device-ioctl gap from
        // carrying the truncate gap's much stronger warning.
        for gap in ALL_GAPS {
            let present = enforcement.gaps().contains(gap);
            assert_eq!(
                description.contains(gap.model_caveat()),
                present,
                "{gap:?} caveat present={} but host gaps={:?}: {description}",
                !present,
                enforcement.gaps()
            );
        }
        // Design notes ride along wherever the host is confined: a refusal the
        // sandbox imposes on purpose surfaces as the same "Permission denied" a
        // boundary denial does, so it must be disclosed on Full hosts too.
        // (Empty off Linux and wherever the ioctl gap exists instead — the
        // exclusivity is pinned in `sandbox::tests`.)
        for note in crate::sandbox::sandbox_design_notes() {
            assert_eq!(
                description.contains(note),
                enforcement.is_enforced(),
                "a design note must appear exactly on confined hosts: {description}"
            );
        }
        if !enforcement.is_enforced() {
            assert!(
                description.contains("NO OS sandbox confinement"),
                "unconfined host must not claim confinement: {description}"
            );
            assert!(!description.contains("sandboxed without network"));
        }
    }
}

/// [`describe`] with fabricated inputs, because the interesting host shapes
/// cannot all be real at once: no CI runner is a Linux 6.10+ kernel, so the
/// "design note present" side would otherwise ship untested — the exact
/// silent-skip trap the gap caveats fell into before 2ae27bf.
#[test]
fn describe_appends_gap_caveats_then_design_notes() {
    use crate::sandbox::{Enforcement, EnforcementGap};

    const NOTE: &str = "DESIGN-NOTE-SENTINEL.";

    // Full host with a designed refusal: body intact, note appended.
    let full = describe(
        SHELL_DESC_CONFINED,
        SHELL_DESC_UNCONFINED,
        &Enforcement::Full,
        &[NOTE],
    );
    assert!(full.starts_with(SHELL_DESC_CONFINED));
    assert!(full.ends_with(NOTE));

    // Partial host: its gap caveat comes first, the note still lands.
    let partial = describe(
        SHELL_DESC_CONFINED,
        SHELL_DESC_UNCONFINED,
        &Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]),
        &[NOTE],
    );
    assert!(partial.contains(EnforcementGap::LandlockTruncate.model_caveat()));
    assert!(partial.ends_with(NOTE));

    // Unconfined host: no sandbox note lands on a body that promises no
    // sandbox — only the sandbox-independent spill sentence joins the body.
    let none = describe(
        SHELL_DESC_CONFINED,
        SHELL_DESC_UNCONFINED,
        &Enforcement::None,
        &[NOTE],
    );
    assert_eq!(none, format!("{SHELL_DESC_UNCONFINED}{SPILL_DESC}"));
}

/// Retention contract for spill runs: stale `run-*` directories are removed,
/// fresh ones and everything that is not a spill run survive, and a missing
/// spill home is a silent no-op. Cutoffs are passed explicitly so the test
/// needs no mtime forgery: "everything is stale" (future cutoff) and
/// "nothing is stale" (epoch cutoff) pin both sides.
#[test]
fn prune_stale_spill_runs_removes_only_stale_run_dirs() {
    let tmp = tempdir().unwrap();
    let home = spill_home(tmp.path());
    let run = home.join("run-1755000000000-42-0");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("job_1.stdout.log"), "log").unwrap();
    let unrelated_dir = home.join("keep-me");
    std::fs::create_dir_all(&unrelated_dir).unwrap();
    let unrelated_file = home.join("run-shaped-file");
    std::fs::write(&unrelated_file, "not a dir").unwrap();

    // Epoch cutoff: nothing can predate it — everything stays.
    prune_stale_spill_runs(&home, std::time::UNIX_EPOCH);
    assert!(run.is_dir(), "fresh runs survive a harmless cutoff");

    // Future cutoff: every run is stale — only run dirs go.
    let future = std::time::SystemTime::now() + Duration::from_secs(3600);
    prune_stale_spill_runs(&home, future);
    assert!(!run.exists(), "stale run dirs are removed");
    assert!(unrelated_dir.is_dir(), "non-run dirs are never touched");
    assert!(unrelated_file.is_file(), "files are never touched");
    assert!(home.is_dir(), "the spill home itself stays");

    // Missing home: silently nothing.
    prune_stale_spill_runs(&tmp.path().join("absent"), future);
}

/// Appends move only a FILE's mtime, never the run directory's — so a run
/// whose dir looks stale but whose newest file is fresher than the cutoff is
/// a still-writing job and must survive. Pinned by pushing the file mtime
/// past the cutoff instead of backdating the dir (portable everywhere).
#[test]
fn prune_keeps_a_run_whose_files_outdate_its_directory() {
    let tmp = tempdir().unwrap();
    let home = spill_home(tmp.path());
    let run = home.join("run-1755000000000-42-1");
    std::fs::create_dir_all(&run).unwrap();
    let log = run.join("job_1.stdout.log");
    std::fs::write(&log, "streaming").unwrap();
    let now = std::time::SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(&log)
        .unwrap()
        .set_modified(now + Duration::from_secs(7200))
        .unwrap();

    // Dir mtime (now) is stale against this cutoff; the file is not.
    prune_stale_spill_runs(&home, now + Duration::from_secs(3600));
    assert!(run.is_dir(), "a fresh file must keep its run alive");

    // Past the file's mtime too: the run is genuinely dead.
    prune_stale_spill_runs(&home, now + Duration::from_secs(3 * 3600));
    assert!(!run.exists(), "stale dir with stale files is removed");
}

// ---------------------------------------------------------------------------
// Unattended execution: a command nobody read runs as the policy's argv, with
// no shell in between (`RunAuthority::Parse`). These tests are the acceptance
// check for that promise; the sandbox is forced off because it is not what is
// under test here.
// ---------------------------------------------------------------------------

/// A registry whose policy trusts `extra` identities on top of the defaults.
#[cfg(unix)]
fn registry_trusting(root: &std::path::Path, extra: &[&str]) -> ToolRegistry {
    let mut registry = registry(root);
    let mut policy = crate::execution_policy::ExecPolicy::default();
    for rule in extra {
        policy = policy.with_trusted_prefix(rule);
    }
    registry.set_policy(policy);
    registry
}

/// A recorder the tests trust by absolute path: one argument per line.
#[cfg(unix)]
fn recorder(dir: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rec");
    std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[cfg(unix)]
async fn run_shell(
    registry: &ToolRegistry,
    arguments: Value,
    decision: Option<ApprovalDecision>,
) -> ToolResult {
    let call = ToolCall::new("call_1", "shell", arguments);
    match registry.run_tool_call(call, decision).await.unwrap() {
        ToolRunOutcome::Result { result } => result,
        ToolRunOutcome::ApprovalRequired { .. } => panic!("unexpected approval request"),
    }
}

/// The stdout lines of a shell result: everything before the first bracketed
/// status/stderr marker.
#[cfg(unix)]
fn stdout_lines(result: &ToolResult) -> Vec<String> {
    result
        .content
        .lines()
        .take_while(|line| !line.starts_with('['))
        .map(str::to_string)
        .collect()
}

/// A trusted command's words reach the program exactly as the policy parsed
/// them, quotes resolved — and they reach it with no shell in between. The
/// second half is the part a shell could fake: `exit` exists only as a shell
/// builtin, so a trusted `exit 3` either fails to start (no such program: the
/// argv went straight to `execve`) or exits 3 (a shell read it). Approved as
/// text, the same line still gets the shell, as before.
#[cfg(unix)]
#[tokio::test]
async fn unattended_commands_run_without_a_shell() {
    let tmp = tempdir().unwrap();
    let rec = recorder(tmp.path());
    let registry = registry_trusting(tmp.path(), &[&rec, "exit"]);

    let trusted = run_shell(
        &registry,
        json!({"command": format!("{rec} one \"two words\" 'three' four\\ five")}),
        None,
    )
    .await;
    assert_eq!(
        trusted.status,
        ToolResultStatus::Success,
        "{}",
        trusted.content
    );
    assert_eq!(
        stdout_lines(&trusted),
        ["one", "two words", "three", "four five"]
    );

    // Nothing named `exit` exists to exec: the policy's argv was not read by a
    // shell. (A shell would have exited 3.)
    let call = ToolCall::new("call_2", "shell", json!({"command": "exit 3"}));
    let error = registry
        .run_tool_call(call, None)
        .await
        .expect_err("a trusted `exit 3` must fail to start rather than run in a shell");
    assert!(
        error.to_string().contains("failed to start command"),
        "{error}"
    );

    // Requoting a trusted command does not move it off the trust rule: the
    // rules read the words the executor runs, so `"exit" 3` is the same command
    // as `exit 3` and gets the same verdict — still no shell, still no such
    // program.
    let call = ToolCall::new("call_3", "shell", json!({"command": "\"exit\" 3"}));
    assert!(!registry.evaluate_tool(&call).requires_approval);
    let error = registry
        .run_tool_call(call, None)
        .await
        .expect_err("requoting must not buy a trusted command a shell");
    assert!(
        error.to_string().contains("failed to start command"),
        "{error}"
    );

    // Approved as text, the shell is exactly what runs it. Arithmetic expansion
    // is something only a shell does, and it is also why this line cannot be
    // trusted (the gate refuses what it cannot parse), so the approval is real.
    let call = ToolCall::new("call_4", "shell", json!({"command": "exit $((1+2))"}));
    assert!(registry.evaluate_tool(&call).requires_approval);
    let plan = registry.evaluate_tool(&call);
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call_with_plan(
            &call,
            Some(ApprovalDecision::Approved),
            plan,
            crate::tool::ToolCx::new().with_authority(crate::tool::RunAuthority::Approved),
        )
        .await
        .unwrap()
    else {
        panic!("expected a result");
    };
    assert_eq!(details(&result)["exit_code"], 3, "{}", result.content);
}

/// `&&` and `;` mean what they mean in the shell — a gated step is skipped
/// while the previous status stands, the run's status is the last executed
/// step's — but the steps are separate `execve`s of the policy's argv.
#[cfg(unix)]
#[tokio::test]
async fn unattended_sequences_follow_the_shell_chain_rules() {
    let tmp = tempdir().unwrap();
    let registry = registry_trusting(tmp.path(), &["true", "false"]);

    let skipped = run_shell(
        &registry,
        json!({"command": "false && printf second; printf third"}),
        None,
    )
    .await;
    assert_eq!(
        skipped.status,
        ToolResultStatus::Success,
        "{}",
        skipped.content
    );
    assert_eq!(stdout_lines(&skipped), ["third"]);
    assert_eq!(details(&skipped)["exit_code"], 0);

    let chained = run_shell(
        &registry,
        json!({"command": "true && printf yes\nprintf again"}),
        None,
    )
    .await;
    assert_eq!(stdout_lines(&chained), ["yesagain"]);

    let failing_tail = run_shell(&registry, json!({"command": "printf a; false"}), None).await;
    assert_eq!(details(&failing_tail)["status"], "failed");
    assert_eq!(details(&failing_tail)["exit_code"], 1);
    assert_eq!(stdout_lines(&failing_tail), ["a"]);

    // A step that cannot start inside a sequence reports and carries on by
    // the chain rules, as the shell would (status 127).
    let missing = run_shell(
        &registry,
        json!({"command": "definitely-not-a-program-xyz && printf no; printf yes"}),
        Some(ApprovalDecision::Approved),
    )
    .await;
    // (approved as text: the shell path — the control for the next call)
    assert!(missing.content.contains("yes") && !missing.content.contains("no\n"));
}

/// Parse authority never falls back to the shell. A command the unattended
/// parser cannot read is refused with an explanation (a tool error, which the
/// runtime records as the call's result) — and the identical text, approved
/// as text, runs through the shell as before.
#[cfg(unix)]
#[tokio::test]
async fn parse_authority_never_falls_back_to_the_shell() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let piped = ToolCall::new("call_1", "shell", json!({"command": "printf a | cat"}));
    let plan = registry.evaluate_tool(&piped);
    assert!(plan.requires_approval, "a pipe is never trusted");

    let refused = registry
        .run_tool_call_with_plan(
            &piped,
            Some(ApprovalDecision::Approved),
            plan.clone(),
            crate::tool::ToolCx::new().with_authority(crate::tool::RunAuthority::Parse),
        )
        .await
        .expect_err("parse authority must refuse what it cannot parse");
    assert!(refused.to_string().contains("without a shell"), "{refused}");

    let approved = registry
        .run_tool_call_with_plan(
            &piped,
            Some(ApprovalDecision::Approved),
            plan,
            crate::tool::ToolCx::new(),
        )
        .await
        .unwrap();
    let ToolRunOutcome::Result { result } = approved else {
        panic!("expected a result");
    };
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );
    assert_eq!(stdout_lines(&result), ["a"]);
}

/// A background job is one process the store can own: a trusted single
/// command starts directly; a trusted chain is refused (a tool error) rather
/// than handed to a shell.
#[cfg(unix)]
#[tokio::test]
async fn unattended_background_jobs_run_exactly_one_command() {
    let tmp = tempdir().unwrap();
    let registry = registry_trusting(tmp.path(), &["sleep"]);
    let single = ToolCall::new(
        "call_1",
        "job",
        json!({"action": "start", "command": "sleep 1"}),
    );
    let ToolRunOutcome::Result { result } = registry.run_tool_call(single, None).await.unwrap()
    else {
        panic!("a trusted job must start without approval");
    };
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );
    assert_eq!(details(&result)["status"], "running");

    let chain = ToolCall::new(
        "call_2",
        "job",
        json!({"action": "start", "command": "sleep 1 && sleep 1"}),
    );
    let refused = registry
        .run_tool_call(chain, None)
        .await
        .expect_err("an unattended chain must be refused, not handed to a shell");
    assert!(
        refused.to_string().contains("exactly one command"),
        "{refused}"
    );
}

/// The exec-recording check for the default trust list: every `printf` line
/// the default policy allows delivers to the program exactly the words the
/// unattended parser produced — one per output line — with no shell in the
/// way to rewrite them. `printf '%s\n'` prints one argument per line, so the
/// program itself is the recorder.
#[cfg(unix)]
#[tokio::test]
async fn default_trusted_commands_deliver_the_parsed_argv_verbatim() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    for spelling in [
        "a b c",
        "\"a b\" c",
        "'c d' e",
        "e\\ f g",
        "\"q\\\"x\" y",
        "'it'\"'\"'s' z",
        "\"\" '' end",
        "a\"b\"c d",
        "--flag=v --other=\"w x\"",
        "hash#inside word # and a comment",
        "\"single 'quotes' inside\" \"double \\\"quotes\\\" inside\"",
    ] {
        let command = format!("printf '%s\\n' {spelling}");
        let plan =
            registry.evaluate_tool(&ToolCall::new("c", "shell", json!({"command": command})));
        assert!(!plan.requires_approval, "{command:?} must be trusted");
        let expected: Vec<String> = crate::execution_policy::parse_unattended(&command)
            .expect("trusted commands parse")
            .into_iter()
            .flat_map(|cmd| cmd.argv.into_iter().skip(2))
            .collect();
        let result = run_shell(&registry, json!({"command": command}), None).await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert_eq!(
            stdout_lines(&result),
            expected,
            "{command:?} reached printf as different words"
        );
    }
}

/// A command that never starts must not pin a `Running` record in the store.
///
/// `ShellTool::run` inserts the record before the first spawn and assigns the
/// real status after the loop, so the two `return Err(..)` paths in between
/// used to leave it `Running` — which `evict_finished_jobs` refuses to evict,
/// by design. Two consequences, and the second is the one users feel: the entry
/// is pinned for the life of the session, AND eviction keeps sizing its work
/// against the whole map, so each pinned entry pushes one more genuinely
/// finished job out of the retention window.
///
/// Unix-only for the same reason as its neighbours: it needs a program word
/// that reliably fails to spawn, and `RunAuthority::Parse` to reach the argv
/// executor rather than a shell's own `command not found`.
#[cfg(unix)]
#[tokio::test]
async fn a_command_that_never_starts_leaves_no_running_record_behind() {
    let tmp = tempdir().unwrap();
    let tools = ShellTools::new(tmp.path())
        .unwrap()
        .with_sandbox(SandboxManager::new().force_sandbox(Some(false)));
    let jobs = tools.jobs.clone();
    let registry = tools.into_registry();

    // Awaits inside, so the borrowed `ToolCall` outlives the call it feeds.
    async fn unattended(
        registry: &ToolRegistry,
        command: &str,
    ) -> Result<ToolRunOutcome, ToolError> {
        let call = ToolCall::new("call_1", "shell", json!({ "command": command }));
        let plan = registry.evaluate_tool(&call);
        registry
            .run_tool_call_with_plan(
                &call,
                Some(ApprovalDecision::Approved),
                plan,
                crate::tool::ToolCx::new().with_authority(crate::tool::RunAuthority::Parse),
            )
            .await
    }

    unattended(&registry, "no-such-binary-xyz")
        .await
        .expect_err("a missing program is still the caller's error");
    let state = jobs
        .get("job_1", "shell")
        .expect("the record is still reachable");
    assert_ne!(
        state.lock().unwrap().status,
        JobStatus::Running,
        "a spawn failure must not pin the record as Running"
    );

    // The consequence that is actually visible: real finished jobs keep their
    // place. Well past the cap, every one of these must still be queryable —
    // with the old behaviour the failures displaced all but one of them.
    let mut succeeded = Vec::new();
    for index in 0..40 {
        let _ = unattended(&registry, &format!("no-such-binary-{index}")).await;
        let outcome = unattended(&registry, "/bin/echo ok")
            .await
            .expect("echo runs");
        let ToolRunOutcome::Result { result } = outcome else {
            panic!("expected a result");
        };
        let job_id = details(&result)["job_id"]
            .as_str()
            .expect("a shell result names its job")
            .to_string();
        succeeded.push(job_id);
    }

    // The cap still bounds the store, so only the newest survive — but they
    // survive by age, not by being crowded out by records that never ran.
    let recent = &succeeded[succeeded.len() - 8..];
    for job_id in recent {
        assert!(
            jobs.get(job_id, "shell").is_ok(),
            "{job_id} was evicted while stuck records held the retention window"
        );
    }
}

/// Windows twin of `shell_trusted_command_runs_without_approval`, and the pin
/// on the trust list being platform-aware.
///
/// `echo` must NOT be auto-trusted here: a trusted command runs as the argv the
/// gate parsed, with no shell in between, and `echo` is a `cmd.exe` builtin
/// that resolves to no file — `sandbox::windows::resolve_executable` refuses it
/// by design rather than routing it back through `cmd /C`. Trusting it made
/// every `echo hi` a spawn failure whose message blamed the OS sandbox. Through
/// the approval path it runs as approved text and works, so the same
/// result/details plumbing is covered.
///
/// Whether it happens to work is not left to the machine: a Windows host with
/// Unix tools on PATH (a CI image with Git for Windows, say) does have an
/// `echo.exe`, so trusting it passed on the runner and failed for users. A
/// trust decision must not depend on that.
#[cfg(windows)]
#[tokio::test]
async fn echo_is_not_auto_trusted_on_windows_and_still_runs_once_approved() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "shell", json!({"command": "echo hello"}));

    let plan = registry.evaluate_tool(&call);
    assert!(
        plan.requires_approval,
        "`echo` names no program on Windows, so it must not be auto-trusted"
    );

    let result = approved(tmp.path(), "shell", json!({"command": "echo hello"})).await;
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );
    assert!(result.content.contains("hello"), "{}", result.content);
    let info = details(&result);
    assert_eq!(info["status"], "completed");
    assert_eq!(info["exit_code"], 0);
    assert_eq!(info["kind"], "foreground");
}
