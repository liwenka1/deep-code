use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use tempfile::tempdir;

use super::*;
use crate::tool::{ApprovalDecision, ToolCall, ToolResult, ToolResultStatus, ToolRunOutcome};

fn registry(root: &Path) -> ToolRegistry {
    workspace_tool_registry(root.to_path_buf()).unwrap()
}

async fn run(root: &Path, name: &str, arguments: Value) -> ToolResult {
    let registry = registry(root);
    let call = ToolCall::new("call_1", name, arguments);
    let ToolRunOutcome::Result { result } = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
        .unwrap()
    else {
        panic!("expected result");
    };
    result
}

#[tokio::test]
async fn read_file_returns_structured_lines() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();

    let result = run(
        tmp.path(),
        "read_file",
        json!({"path": "notes.txt", "start_line": 2, "max_lines": 1}),
    )
    .await;
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["path"], "notes.txt");
    assert_eq!(output["lines"][0]["line"], 2);
    assert_eq!(output["lines"][0]["text"], "two");
    assert_eq!(output["truncated"], true);
}

#[tokio::test]
async fn list_dir_returns_structured_entries() {
    let tmp = tempdir().unwrap();
    fs::create_dir(tmp.path().join("src")).unwrap();
    fs::write(tmp.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();

    let result = run(tmp.path(), "list_dir", json!({"path": "src"})).await;
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["path"], "src");
    assert_eq!(output["entries"][0]["name"], "lib.rs");
    assert_eq!(output["entries"][0]["kind"], "file");
}

#[tokio::test]
async fn grep_files_returns_matches_with_context() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("main.rs"), "alpha\nneedle\nomega\n").unwrap();

    let result = run(
        tmp.path(),
        "grep_files",
        json!({"pattern": "needle", "context_lines": 1}),
    )
    .await;
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["matches"][0]["path"], "main.rs");
    assert_eq!(output["matches"][0]["line_number"], 2);
    assert_eq!(output["matches"][0]["context_before"][0]["text"], "alpha");
    assert_eq!(output["matches"][0]["context_after"][0]["text"], "omega");
}

#[tokio::test]
async fn write_file_requires_approval_and_writes_after_approval() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "write_file",
        json!({"path": "new.txt", "content": "hello"}),
    );

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

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn apply_patch_replaces_exactly_once_after_approval() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("lib.rs"), "fn old() {}\n").unwrap();

    let result = run(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "old", "new": "new"}),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
        "fn new() {}\n"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["match"],
        "exact"
    );
}

/// Run a tool call expecting it to fail, returning the error's message.
async fn run_err(root: &Path, name: &str, arguments: Value) -> String {
    let registry = registry(root);
    let call = ToolCall::new("call_1", name, arguments);
    match registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await
    {
        Err(ToolError::InvalidArguments { message, .. }) => message,
        Err(other) => panic!("expected InvalidArguments, got {other:?}"),
        Ok(_) => panic!("expected an error"),
    }
}

#[tokio::test]
async fn apply_patch_fuzzy_matches_indentation_and_uses_new_indent() {
    let tmp = tempdir().unwrap();
    // Middle line is tab-indented; `old` carries a 4-space indent, so there is
    // no exact substring — only the indentation-insensitive layer can match.
    fs::write(
        tmp.path().join("lib.rs"),
        "fn f() {\n\tlet x = compute();\n}\n",
    )
    .unwrap();

    let result = run(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "    let x = compute();", "new": "    let y = 42;"}),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    // The original tab indent is replaced by `new`'s indentation; surrounding
    // lines are untouched.
    assert_eq!(
        fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
        "fn f() {\n    let y = 42;\n}\n"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["match"],
        "fuzzy-indent"
    );
}

#[tokio::test]
async fn apply_patch_fuzzy_matches_smart_quotes() {
    let tmp = tempdir().unwrap();
    // File uses curly quotes; the model's `old` uses ASCII quotes.
    fs::write(tmp.path().join("s.rs"), "let s = \u{201c}hi\u{201d};\n").unwrap();

    let result = run(
        tmp.path(),
        "apply_patch",
        json!({"path": "s.rs", "old": "\"hi\"", "new": "\"bye\""}),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(tmp.path().join("s.rs")).unwrap(),
        "let s = \"bye\";\n"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["match"],
        "fuzzy-punct"
    );
}

#[tokio::test]
async fn apply_patch_preserves_crlf_and_untouched_bytes() {
    let tmp = tempdir().unwrap();
    // CRLF line endings and curly quotes on an untouched line must survive an
    // exact edit elsewhere — the splice only rewrites the matched range.
    let original = "let keep = \u{201c}x\u{201d};\r\nfn old() {}\r\n";
    fs::write(tmp.path().join("lib.rs"), original).unwrap();

    run(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "old", "new": "new"}),
    )
    .await;

    assert_eq!(
        fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
        "let keep = \u{201c}x\u{201d};\r\nfn new() {}\r\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_non_unique_old_with_recovery_hint() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("dup.rs"), "x = 1;\nx = 1;\n").unwrap();

    let message = run_err(
        tmp.path(),
        "apply_patch",
        json!({"path": "dup.rs", "old": "x = 1;", "new": "x = 2;"}),
    )
    .await;

    assert!(message.contains("matched 2 places"), "got: {message}");
    assert!(message.contains("Recovery"), "got: {message}");
    // File is untouched on a rejected edit.
    assert_eq!(
        fs::read_to_string(tmp.path().join("dup.rs")).unwrap(),
        "x = 1;\nx = 1;\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_missing_old_with_recovery_hint() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("lib.rs"), "fn f() {}\n").unwrap();

    let message = run_err(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "does_not_exist", "new": "z"}),
    )
    .await;

    assert!(message.contains("not found"), "got: {message}");
    assert!(message.contains("Recovery"), "got: {message}");
}

#[tokio::test]
async fn apply_patch_rejects_identical_old_and_new() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("lib.rs"), "fn f() {}\n").unwrap();

    let message = run_err(
        tmp.path(),
        "apply_patch",
        json!({"path": "lib.rs", "old": "f", "new": "f"}),
    )
    .await;

    assert!(message.contains("identical"), "got: {message}");
}

#[tokio::test]
async fn rejects_parent_traversal() {
    let tmp = tempdir().unwrap();
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "read_file", json!({"path": "../secret.txt"}));

    assert!(matches!(
        registry.run_tool_call(call, None).await,
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_paths() {
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
        registry.run_tool_call(call, None).await,
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_rejects_existing_target_symlink() {
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
        registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .await,
        Err(ToolError::InvalidArguments { .. })
    ));
    assert_eq!(fs::read_to_string(outside_file).unwrap(), "secret");
}
