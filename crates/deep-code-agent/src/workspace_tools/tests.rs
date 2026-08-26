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

/// Behavioral contract for shell-output spill files (which live under the
/// workspace's `.deep-code/spill/`): the default walk must NOT surface them —
/// logs polluting code searches would be worse than no spill at all — while
/// an explicit path into the directory (what the truncation note tells the
/// model to do) must search them, and `read_file` must page through them.
#[tokio::test]
async fn spill_files_are_hidden_from_default_grep_but_reachable_explicitly() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("main.rs"), "NEEDLE in code\n").unwrap();
    let spill = tmp.path().join(".deep-code/spill/run-1");
    fs::create_dir_all(&spill).unwrap();
    fs::write(spill.join("job_1.stdout.log"), "NEEDLE in log\n").unwrap();

    // Default walk: only the source hit; the hidden `.deep-code` never shows.
    let result = run(tmp.path(), "grep_files", json!({"pattern": "NEEDLE"})).await;
    let output: Value = serde_json::from_str(&result.content).unwrap();
    let matched: Vec<&str> = output["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(matched, vec!["main.rs"], "spill must not pollute code grep");

    // Explicit path into the spill dir: the log line is found.
    let result = run(
        tmp.path(),
        "grep_files",
        json!({"pattern": "NEEDLE", "path": ".deep-code/spill"}),
    )
    .await;
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["matches"][0]["line"], "NEEDLE in log");

    // read_file reaches the same file — the granted-root resolution covers
    // the spill path with no special casing.
    let result = run(
        tmp.path(),
        "read_file",
        json!({"path": ".deep-code/spill/run-1/job_1.stdout.log"}),
    )
    .await;
    assert_eq!(result.status, ToolResultStatus::Success);
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["lines"][0]["text"], "NEEDLE in log");
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

/// Files over the size limit are refused by the walk — but refused OUT LOUD
/// and BY NAME. A silent skip reads as "searched everything, found nothing",
/// and the needle buried in the big file below is exactly the match that lie
/// would hide. `read_file` already reports its limit explicitly; this pins
/// the same honesty onto grep. The limit in the note is asserted against the
/// constant, not a literal, so bumping `MAX_FILE_BYTES` cannot leave the
/// prose lying while this test stays green.
#[tokio::test]
async fn grep_files_reports_oversized_files_instead_of_skipping_silently() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("small.txt"), "needle in the small file\n").unwrap();
    let mut big = String::from("needle at the head\n");
    big.push_str(&"x".repeat(usize::try_from(crate::workspace_tools::MAX_FILE_BYTES).unwrap()));
    fs::write(tmp.path().join("big.log"), big).unwrap();

    let result = run(tmp.path(), "grep_files", json!({"pattern": "needle"})).await;
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["skipped_oversized"], 1);
    assert_eq!(
        output["skipped_oversized_paths"],
        json!(["big.log"]),
        "the skipped file must be named, or the caller cannot go look: {output}"
    );
    assert!(
        output["note"]
            .as_str()
            .expect("a skipped file must leave a note")
            .contains(&format!("{} MiB", crate::workspace_tools::MAX_FILE_MIB)),
        "note must name the limit: {output}"
    );
    // The big file was truly not searched: only the small file matched.
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(output["matches"][0]["path"], "small.txt");
}

/// The other refusal ledger: a file that cannot be read (here: invalid UTF-8)
/// used to vanish without a trace — not in `files_searched`, not in any
/// count — which is the same "searched everything" lie in a different branch.
#[tokio::test]
async fn grep_files_reports_unreadable_files_instead_of_skipping_silently() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("small.txt"), "needle in the small file\n").unwrap();
    fs::write(tmp.path().join("binary.bin"), [0xFF, 0xFE, b'n', 0x80]).unwrap();

    let result = run(tmp.path(), "grep_files", json!({"pattern": "needle"})).await;
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["skipped_unreadable"], 1, "{output}");
    assert_eq!(
        output["skipped_unreadable_paths"],
        json!(["binary.bin"]),
        "{output}"
    );
    assert!(
        output["note"]
            .as_str()
            .expect("a skipped file must leave a note")
            .contains("1 unreadable"),
        "{output}"
    );
    assert_eq!(output["files_searched"], 1, "{output}");
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
}

/// A directory the walk itself cannot open takes its ENTIRE subtree with it —
/// by far the largest refusal grep can make, and until now the only one that
/// vanished without a count or a path. The per-file ledgers were honest while
/// the level above them was not.
#[cfg(unix)]
#[tokio::test]
async fn grep_files_reports_an_unreadable_directory() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("small.txt"), "needle in the small file\n").unwrap();
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("buried.txt"), "needle\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // root ignores the mode bits, so the refusal this test needs never happens
    // there; skip rather than assert something the environment cannot produce.
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: this user can read a 0000 directory (root?)");
        return;
    }

    let result = run(tmp.path(), "grep_files", json!({"pattern": "needle"})).await;
    // Restore before asserting: a panic here must not leave a tempdir that
    // cannot be cleaned up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();

    assert_eq!(output["skipped_unreadable"], 1, "{output}");
    assert_eq!(
        output["skipped_unreadable_paths"],
        json!(["locked"]),
        "the unreadable directory must be named: {output}"
    );
    assert!(
        output["note"]
            .as_str()
            .expect("a skipped directory must leave a note")
            .contains("1 unreadable"),
        "{output}"
    );
    // The buried match was genuinely not found — that is the point of saying so.
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(output["matches"][0]["path"], "small.txt");
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

/// The real CRLF case, which the single-token test above never exercised: a
/// *multi-line* `old` copied from `read_file`, whose `str::lines` split drops
/// every `\r`. Exact compared `\n` against `\r\n`, the indentation layer only
/// strips *leading* whitespace, and punctuation folding does not touch `\r` — so
/// all three layers missed, and the error told the model to "copy `old` verbatim
/// from read_file", which is exactly what it had just done.
#[tokio::test]
async fn apply_patch_matches_multiline_old_in_a_crlf_file() {
    let tmp = tempdir().unwrap();
    let original = "fn a() {\r\n    let x = 1;\r\n    let y = 2;\r\n}\r\n";
    fs::write(tmp.path().join("lib.rs"), original).unwrap();

    let result = run(
        tmp.path(),
        "apply_patch",
        // Exactly what read_file hands the model: LF only.
        json!({
            "path": "lib.rs",
            "old": "    let x = 1;\n    let y = 2;",
            "new": "    let x = 10;\n    let y = 20;",
        }),
    )
    .await;
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );

    // Patched — and still CRLF throughout, including the rewritten lines, so a
    // successful edit does not leave an LF island in a CRLF file.
    assert_eq!(
        fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
        "fn a() {\r\n    let x = 10;\r\n    let y = 20;\r\n}\r\n"
    );
}

/// Creating a new module is routine, and refusing it pushed the model into a
/// separately-approved `mkdir -p` for a directory `write_file` was already
/// authorized to create.
#[tokio::test]
async fn write_file_creates_missing_parent_directories() {
    let tmp = tempdir().unwrap();

    let result = run(
        tmp.path(),
        "write_file",
        json!({"path": "src/deep/new_mod.rs", "content": "pub fn f() {}\n"}),
    )
    .await;
    assert_eq!(
        result.status,
        ToolResultStatus::Success,
        "{}",
        result.content
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("src/deep/new_mod.rs")).unwrap(),
        "pub fn f() {}\n"
    );

    // Creating parents must not become an escape hatch.
    run_err(
        tmp.path(),
        "write_file",
        json!({"path": "../outside/x.rs", "content": "x"}),
    )
    .await;
    assert!(
        !tmp.path().parent().unwrap().join("outside").exists(),
        "must not create directories outside the workspace"
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

// Skip/triage policy (unix bugs panic, Windows may lack the privilege,
// DEEPCODE_REQUIRE_SYMLINKS hardens CI) lives in `crate::test_symlinks`.
use crate::test_symlinks::symlink_file_for_test;

#[tokio::test]
async fn rejects_symlink_paths() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    if !symlink_file_for_test(
        &outside.path().join("secret.txt"),
        &tmp.path().join("link.txt"),
    ) {
        return;
    }
    let registry = registry(tmp.path());
    let call = ToolCall::new("call_1", "read_file", json!({"path": "link.txt"}));

    assert!(matches!(
        registry.run_tool_call(call, None).await,
        Err(ToolError::InvalidArguments { .. })
    ));
}

#[tokio::test]
async fn write_file_rejects_existing_target_symlink() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "secret").unwrap();
    if !symlink_file_for_test(&outside_file, &tmp.path().join("link.txt")) {
        return;
    }

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

/// The complement of the test above, and the case that actually escaped: the
/// link's target does not exist yet.
///
/// `Path::exists()` follows symlinks, so a dangling link reported "no such
/// path" and `resolve_for_write` took its non-existent branch — which starts
/// its walk at `parent` and never stats the leaf. `fs::write` then created the
/// target. Planting the link is a permitted write inside the root, the file
/// tools run in-process where no sandbox sees them, and `write_file`
/// auto-approves under `accept_edits`, so this was an unattended arbitrary
/// write with the panel captioned "new file notes.txt".
///
/// Asserts on the filesystem, not on the error type: what matters is that
/// nothing was created outside the root, whichever way the refusal is spelled.
#[tokio::test]
async fn write_file_rejects_dangling_target_symlink() {
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("authorized_keys");
    if !symlink_file_for_test(&outside_file, &tmp.path().join("notes.txt")) {
        return;
    }

    let registry = registry(tmp.path());
    let call = ToolCall::new(
        "call_1",
        "write_file",
        json!({"path": "notes.txt", "content": "pwned"}),
    );

    let outcome = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await;

    assert!(
        !outside_file.exists(),
        "write escaped to {} (outside every granted root)",
        outside_file.display()
    );
    assert!(matches!(outcome, Err(ToolError::InvalidArguments { .. })));
}
