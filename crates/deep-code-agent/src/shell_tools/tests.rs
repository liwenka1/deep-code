use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::tempdir;

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

#[tokio::test]
async fn shell_run_requires_approval_and_returns_output() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    // `echo hello` works on both sh and cmd, so the test is cross-platform.
    let call = ToolCall::new("call_1", "shell_run", json!({"command": "echo hello"}));

    assert!(matches!(
        registry.run_tool_call(call.clone(), None).await.unwrap(),
        ToolRunOutcome::ApprovalRequired { .. }
    ));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(output["stdout"].as_str().unwrap().contains("hello"));
    assert_eq!(output["status"], "completed");
    assert_eq!(output["kind"], "foreground");
    let job_id = output["job_id"].as_str().unwrap();
    let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).await.unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["kind"], "foreground");
}

#[tokio::test]
async fn shell_run_reports_failure() {
    let tmp = tempdir().unwrap();
    let result = approved(tmp.path(), "shell_run", json!({"command": "exit 7"})).await;
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(output["exit_code"], 7);
}

#[tokio::test]
async fn shell_run_times_out() {
    let tmp = tempdir().unwrap();
    let result = approved(
        tmp.path(),
        "shell_run",
        json!({"command": "sleep 1", "timeout_ms": 1}),
    )
    .await;
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "timed_out");
}

#[tokio::test]
async fn long_shell_run_returns_cancellable_job_id() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "shell_run", json!({"command": "sleep 2"}));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "running");
    let job_id = output["job_id"].as_str().unwrap();

    let cancel = ToolCall::new("call_2", "job_cancel", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(cancel, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "cancelled");
}

#[tokio::test]
async fn running_shell_run_preserves_timeout_on_job_status() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "shell_run",
        json!({"command": "sleep 1", "timeout_ms": 150}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "running");
    let job_id = output["job_id"].as_str().unwrap();

    thread::sleep(Duration::from_millis(220));
    let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).await.unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "timed_out");
}

#[tokio::test]
async fn completed_shell_run_is_not_marked_timed_out_by_late_status_check() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "shell_run",
        json!({"command": "sleep 0.2", "timeout_ms": 1000}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "running");
    let job_id = output["job_id"].as_str().unwrap();

    thread::sleep(Duration::from_millis(1_100));
    let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).await.unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "completed");
}

#[tokio::test]
async fn shell_run_times_out_without_polling_before_deadline() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "shell_run",
        json!({"command": "sleep 1", "timeout_ms": 150}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "running");
    let job_id = output["job_id"].as_str().unwrap();

    thread::sleep(Duration::from_millis(300));
    let status = ToolCall::new("call_2", "job_status", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(status, None).await.unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "timed_out");
}

#[tokio::test]
async fn shell_rejects_cwd_escape() {
    let tmp = tempdir().unwrap();
    let call = ToolCall::new(
        "call_1",
        "shell_run",
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
    let call = ToolCall::new(
        "call_1",
        "job_start",
        json!({"command": "printf hello && sleep 2"}),
    );
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    let job_id = output["job_id"].as_str().unwrap();

    thread::sleep(Duration::from_millis(50));
    let tail = ToolCall::new("call_2", "job_tail", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry.run_tool_call(tail, None).await.unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["stdout_tail"], "hello");

    let cancel = ToolCall::new("call_3", "job_cancel", json!({"job_id": job_id}));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(cancel, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["status"], "cancelled");
}
