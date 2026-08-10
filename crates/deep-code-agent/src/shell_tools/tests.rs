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

/// The model must never be told it is confined when it is not. Both tool
/// descriptions are picked from the host's real capability, so this asserts the
/// two stay in agreement — on Windows (Job Object: no fs/network confinement)
/// the "runs sandboxed without network" wording would be a lie, and a model that
/// believes it is offline will not declare the network it silently already has.
#[test]
fn tool_descriptions_match_actual_sandbox_capability() {
    use crate::tool::Tool;

    let confined = crate::sandbox::sandbox_confines_network();
    let shell = ShellTool::new(
        WorkspacePolicy::new(std::path::Path::new(".")).unwrap(),
        JobStore::default(),
        SandboxManager::new(),
    );
    let job = JobTool::new(
        WorkspacePolicy::new(std::path::Path::new(".")).unwrap(),
        JobStore::default(),
        SandboxManager::new(),
    );

    for description in [shell.description(), job.description()] {
        if confined {
            assert!(
                description.contains("sandboxed without network"),
                "confined host must advertise confinement: {description}"
            );
            assert!(!description.contains("NO OS sandbox"));
        } else {
            assert!(
                description.contains("NO OS sandbox confinement"),
                "unconfined host must not claim confinement: {description}"
            );
            assert!(!description.contains("run sandboxed without network"));
        }
    }
}
