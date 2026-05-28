use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::tool::{ApprovalDecision, ToolResultStatus, ToolRunOutcome};

fn registry(root: &Path) -> ToolRegistry {
    workspace_tool_registry(root.to_path_buf()).unwrap()
}

fn run(root: &Path, name: &str, arguments: Value) -> ToolResult {
    let registry = registry(root);
    let call = ToolCall::new("call_1", name, arguments);
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .unwrap()
    else {
        panic!("expected result");
    };
    result
}

#[test]
fn read_file_returns_structured_lines() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();

    let result = run(
        tmp.path(),
        "read_file",
        json!({"path": "notes.txt", "start_line": 2, "max_lines": 1}),
    );
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["path"], "notes.txt");
    assert_eq!(output["lines"][0]["line"], 2);
    assert_eq!(output["lines"][0]["text"], "two");
    assert_eq!(output["truncated"], true);
}

#[test]
fn list_dir_returns_structured_entries() {
    let tmp = tempdir().unwrap();
    fs::create_dir(tmp.path().join("src")).unwrap();
    fs::write(tmp.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();

    let result = run(tmp.path(), "list_dir", json!({"path": "src"}));
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["path"], "src");
    assert_eq!(output["entries"][0]["name"], "lib.rs");
    assert_eq!(output["entries"][0]["kind"], "file");
}

#[test]
fn grep_files_returns_matches_with_context() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("main.rs"), "alpha\nneedle\nomega\n").unwrap();

    let result = run(
        tmp.path(),
        "grep_files",
        json!({"pattern": "needle", "context_lines": 1}),
    );
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["matches"][0]["path"], "main.rs");
    assert_eq!(output["matches"][0]["line_number"], 2);
    assert_eq!(output["matches"][0]["context_before"][0]["text"], "alpha");
    assert_eq!(output["matches"][0]["context_after"][0]["text"], "omega");
}

#[test]
fn write_file_requires_approval_and_writes_after_approval() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "write_file",
        json!({"path": "new.txt", "content": "hello"}),
    );

    assert!(matches!(
        registry.run_tool_call(call.clone(), None).unwrap(),
        ToolRunOutcome::ApprovalRequired { .. }
    ));
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .unwrap()
    else {
        panic!("expected result");
    };

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn apply_patch_replaces_exactly_once_after_approval() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("lib.rs"), "fn old() {}\n").unwrap();

    let result = run(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "old", "new": "new"}),
    );

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
        "fn new() {}\n"
    );
}

#[test]
fn rejects_parent_traversal() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "read_file", json!({"path": "../secret.txt"}));

    assert!(matches!(
        registry.run_tool_call(call, None),
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_paths() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        tmp.path().join("link.txt"),
    )
    .unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "read_file", json!({"path": "link.txt"}));

    assert!(matches!(
        registry.run_tool_call(call, None),
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[cfg(unix)]
#[test]
fn write_file_rejects_existing_target_symlink() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "secret").unwrap();
    symlink(&outside_file, tmp.path().join("link.txt")).unwrap();

    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "write_file",
        json!({"path": "link.txt", "content": "leak"}),
    );

    assert!(matches!(
        registry.run_tool_call(call, Some(ApprovalDecision::Approved)),
        Err(ToolError::InvalidArguments { .. })
    ));
    assert_eq!(fs::read_to_string(outside_file).unwrap(), "secret");
}
