use std::path::PathBuf;

use deep_code_agent::{ExchangeResult, SessionEntry, SessionRecord, ToolCallPayload, ToolExchange};

use super::*;

fn call(id: &str, name: &str) -> ToolCallPayload {
    ToolCallPayload {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: deep_code_agent::ToolCallFunctionPayload {
            name: name.to_string(),
            arguments: "{\"message\":\"hi\"}".to_string(),
        },
    }
}

/// The grep summary line must carry the refusal ledgers: "0 matches
/// across N files" with skipped files hidden is the misread the counts
/// were added to prevent — for the human this line is all there is.
#[test]
fn grep_summary_surfaces_skipped_files() {
    // Every ledger the tool emits, each named: the model is told which
    // bucket a refusal landed in, and one summed integer took that back
    // — "the boundary refused it" and "grep could not read it" are
    // different problems with different fixes.
    let with_skips = summarize_tool_result(
        r#"{"path":"logs","files_searched":5,"matches":[],"truncated":false,
                "skipped_oversized":2,"skipped_binary":3,"skipped_symlinks":4,
                "skipped_unreadable":1}"#,
    );
    assert_eq!(
        with_skips,
        "logs: 0 matches across 5 files (truncated=false, \
             skipped oversized=2 binary=3 symlinks=4 unreadable=1)"
    );

    // The tool's "at least" hedge has to reach the human too: without it a
    // floor reads as a census, which is the same misread one level up.
    let hedged = summarize_tool_result(
        r#"{"path":"logs","files_searched":5,"matches":[],"truncated":false,
                "skipped_unreadable":1,
                "note":"not searched: at least 1 unreadable path(s)"}"#,
    );
    assert_eq!(
        hedged,
        "logs: 0 matches across 5 files (truncated=false, \
             skipped unreadable=1 (at least))"
    );

    // A grep of the workspace root has its prefix stripped to "", which
    // `unwrap_or` does not catch — the line used to open with a bare colon.
    let rooted =
        summarize_tool_result(r#"{"path":"","files_searched":1,"matches":[],"truncated":false}"#);
    assert_eq!(rooted, ".: 0 matches across 1 files (truncated=false)");
    let clean = summarize_tool_result(
        r#"{"path":"src","files_searched":5,"matches":[],"truncated":false,
                "skipped_oversized":0,"skipped_binary":0,"skipped_symlinks":0,
                "skipped_unreadable":0}"#,
    );
    assert_eq!(clean, "src: 0 matches across 5 files (truncated=false)");
}

#[test]
fn hydrate_history_keeps_assistant_tool_calls_and_results() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("hi")));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: call("call_1", "mock_echo"),
                result: Some(ExchangeResult {
                    content: "mock_echo: hi".to_string(),
                    status: ToolResultStatus::Denied,
                }),
            }],
        )));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::compaction(
            "older conversation summary",
            2,
        )));
    let mut turn = deep_code_agent::TurnRecord::new();
    turn.started_at_ms = 10;
    record.turns.push(turn);
    let mut checkpoint = deep_code_agent::CheckpointRecord::new(
        deep_code_agent::CheckpointId("checkpoint_1".to_string()),
        "before_turn",
    );
    checkpoint.created_at_ms = 15;
    record.checkpoints.push(checkpoint);

    let cells = hydrate_history(&record);
    assert!(matches!(cells[0], HistoryCell::User { .. }));
    assert!(matches!(
        &cells[1],
        HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
    ));
    // Status now comes structurally from the exchange — no silent
    // Success fallback.
    assert!(matches!(
        &cells[2],
        HistoryCell::ToolResult {
            tool_name,
            status,
            summary,
        } if tool_name == "mock_echo"
            && *status == ToolResultStatus::Denied
            && summary.contains("mock_echo")
    ));
    assert!(cells.iter().any(|cell| matches!(
        cell,
        HistoryCell::Compaction { metadata, summary }
            if metadata.as_deref() == Some("archived=2")
                && summary == "older conversation summary"
    )));
    assert!(cells.iter().any(|cell| matches!(
        cell,
        HistoryCell::Checkpoint { id, .. } if id == "checkpoint_1"
    )));
}

#[test]
fn hydrate_history_restores_reasoning_content() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("hi")));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "answer",
            Some("thinking".to_string()),
            Vec::new(),
        )));

    let cells = hydrate_history(&record);
    assert!(matches!(
        &cells[1],
        HistoryCell::Reasoning { text } if text == "thinking"
    ));
    assert!(matches!(
        &cells[2],
        HistoryCell::Assistant { text } if text == "answer"
    ));
}

#[test]
fn hydrate_history_renders_pending_exchange_as_call_only() {
    // An interrupted exchange (result never recorded) shows the call but
    // fabricates no result line.
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("go")));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange::pending(call("call_1", "shell"))],
        )));

    let cells = hydrate_history(&record);
    assert!(matches!(
        &cells[1],
        HistoryCell::ToolCall { tool_name, .. } if tool_name == "shell"
    ));
    assert!(
        !cells
            .iter()
            .any(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
    );
}

#[test]
fn tool_call_renders_compact_single_line() {
    let tool = HistoryCell::ToolCall {
        tool_name: "shell".to_string(),
        arguments: "{\"command\":\n  \"grep foo\"}".to_string(),
        risk_level: None,
        requires_sandbox: None,
        approval: ToolApprovalState::NotRequired,
        running_for_secs: None,
    };
    let lines = tool.lines(Lang::Zh);
    assert_eq!(lines.len(), 1, "tool call must be one line");
    assert!(lines[0].starts_with("shell  "));
    // Whitespace/newlines collapsed; no Risk/Approval/Sandbox noise.
    assert!(!lines[0].contains('\n'));
    assert!(!lines[0].contains("Risk"));
    assert!(!lines[0].contains('['), "ungated call carries no badge");
    assert!(!lines[0].contains("· "), "flushed call carries no clock");

    let gated = HistoryCell::ToolCall {
        tool_name: "write_file".to_string(),
        arguments: "{}".to_string(),
        risk_level: Some(deep_code_agent::RiskLevel::Medium),
        requires_sandbox: Some(false),
        approval: ToolApprovalState::Approved,
        running_for_secs: None,
    };
    assert!(gated.lines(Lang::Zh)[0].ends_with("[已批准]"));
    assert!(gated.lines(Lang::En)[0].ends_with("[approved]"));

    // A still-running call (transcript preview) shows its elapsed clock
    // between args and badge.
    let running = HistoryCell::ToolCall {
        tool_name: "agent".to_string(),
        arguments: "{\"role\":\"explore\"}".to_string(),
        risk_level: None,
        requires_sandbox: None,
        approval: ToolApprovalState::NotRequired,
        running_for_secs: Some(47),
    };
    assert!(running.lines(Lang::Zh)[0].contains("· 47s"));
}

#[test]
fn tool_result_renders_compact_single_line() {
    let result = HistoryCell::ToolResult {
        tool_name: "shell".to_string(),
        status: ToolResultStatus::Success,
        summary: "ok\nmulti\nline".to_string(),
    };
    let lines = result.lines(Lang::Zh);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("ok multi line"));
}

#[test]
fn tool_call_lines_truncate_long_fields() {
    let long = "x".repeat(500);
    let tool = HistoryCell::ToolCall {
        tool_name: "write_file".to_string(),
        arguments: long,
        risk_level: None,
        requires_sandbox: None,
        approval: ToolApprovalState::NotRequired,
        running_for_secs: None,
    };
    assert!(
        tool.lines(Lang::Zh)
            .iter()
            .any(|line| line.contains("(truncated)"))
    );
}

#[test]
fn checkpoint_lines_include_restore_command() {
    let cell = HistoryCell::Checkpoint {
        id: "checkpoint_1".to_string(),
        label: "before_turn".to_string(),
    };
    assert!(
        cell.lines(Lang::Zh)
            .iter()
            .any(|line| line == "恢复: /restore checkpoint_1")
    );
    assert!(
        cell.lines(Lang::En)
            .iter()
            .any(|line| line == "Restore: /restore checkpoint_1")
    );
}
