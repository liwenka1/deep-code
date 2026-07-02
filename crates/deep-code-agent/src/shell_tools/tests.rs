use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::tool::{ApprovalDecision, ToolCall, ToolResult, ToolResultStatus, ToolRunOutcome};

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
    result.details.as_ref().expect("shell results carry details")
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
        json!({"command": "sleep 5", "timeout_secs": 1}),
    )
    .await;
    assert!(result.content.contains("timed out"));
    assert_eq!(details(&result)["status"], "timed_out");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "timeout must not wait for the child's natural exit"
    );
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
    let call = ToolCall::new("call_1", "shell", json!({"command": "sleep 5"}));

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
    // `printf ` is a trusted prefix, so action=start runs without approval.
    let call = ToolCall::new(
        "call_1",
        "job",
        json!({"action": "start", "command": "printf hello && sleep 2"}),
    );
    let ToolRunOutcome::Result { result } = registry.run_tool_call(call, None).await.unwrap()
    else {
        panic!("expected result");
    };
    assert!(result.content.contains("started job_"));
    let job_id = details(&result)["job_id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(80)).await;
    let tail = ToolCall::new(
        "call_2",
        "job",
        json!({"action": "tail", "job_id": job_id}),
    );
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
