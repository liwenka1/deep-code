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
        HistoryCell::ToolResult { status, summary }
            if *status == ToolResultStatus::Denied && summary.contains("mock_echo")
    ));
    assert!(cells.iter().any(|cell| matches!(
        cell,
        // A replayed cell can say what was folded but not what it bought: the
        // session record stores no token counts.
        HistoryCell::Compaction {
            archived_entries: 2,
            context_tokens: None,
            summary,
        } if summary == "older conversation summary"
    )));
    assert!(cells.iter().any(|cell| matches!(
        cell,
        HistoryCell::Checkpoint { id, .. } if id == "checkpoint_1"
    )));
}

/// The two things the compaction cell has to answer: what got folded, and — the
/// question that had no answer before — what the model is looking at now.
#[test]
fn a_compaction_cell_says_what_was_folded_and_what_the_model_now_sees() {
    let measured = HistoryCell::Compaction {
        archived_entries: 3,
        context_tokens: Some((120_000, 45_000)),
        summary: "- 用户 / user: 做点事".to_string(),
    };
    let lines = measured.lines(Lang::Zh);

    assert_eq!(lines.len(), 3, "{lines:?}");
    // The count is entries and says so; the delta is what tells the user the
    // fold actually bought something.
    assert!(lines[0].contains("3 条记录"), "{}", lines[0]);
    assert!(
        lines[0].contains("120000") && lines[0].contains("45000"),
        "{}",
        lines[0]
    );
    // The line that resolves the confusion: the block BELOW is the model's
    // context, and the originals are still above it in the transcript.
    assert!(lines[1].contains("模型当前看到"), "{}", lines[1]);
    assert_eq!(lines[2], "- 用户 / user: 做点事");

    // A cell replayed from a session record carries no token counts, so it
    // states what it knows and no more.
    let replayed = HistoryCell::Compaction {
        archived_entries: 2,
        context_tokens: None,
        summary: "…".to_string(),
    };
    let lines = replayed.lines(Lang::En);
    assert!(lines[0].contains("2 record"), "{}", lines[0]);
    assert!(
        !lines[0].contains("tokens"),
        "no numbers, no token delta: {}",
        lines[0]
    );
}

/// Compaction trims `record.entries` and leaves `record.turns` alone, so the
/// two are aligned only at their end. Counting user entries from the front
/// rendered the session's oldest checkpoints — the ones the 20-snapshot disk
/// cap has already deleted — against its newest turns, each with a
/// `/restore <id>` hint pointing at the wrong snapshot.
#[test]
fn hydrate_history_maps_checkpoints_to_turns_after_compaction() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "sys");
    // A session that ran five turns and was then compacted: only the last two
    // turns' entries survive, behind the compaction banner.
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::compaction("older", 6)));
    for index in 3..5 {
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user(format!("u{index}"))));
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::assistant(
                format!("a{index}"),
                None,
                Vec::new(),
            )));
    }
    for index in 0..5u64 {
        let mut turn = deep_code_agent::TurnRecord::new();
        turn.started_at_ms = 10 * (index + 1);
        record.turns.push(turn);
        let mut checkpoint = deep_code_agent::CheckpointRecord::new(
            deep_code_agent::CheckpointId(format!("cp_{index}")),
            "before_turn",
        );
        checkpoint.created_at_ms = 10 * (index + 1) + 1;
        record.checkpoints.push(checkpoint);
    }

    let cells = hydrate_history(&record);
    let ids: Vec<&str> = cells
        .iter()
        .filter_map(|cell| match cell {
            HistoryCell::Checkpoint { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["cp_3", "cp_4"],
        "the two retained turns must carry their own checkpoints"
    );
    // And they sit inside the turn they belong to, not ahead of it.
    let position = |needle: &str| {
        cells
            .iter()
            .position(|cell| match cell {
                HistoryCell::User { text } => text == needle,
                HistoryCell::Checkpoint { id, .. } => id == needle,
                _ => false,
            })
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    assert!(position("u3") < position("cp_3"));
    assert!(position("cp_3") < position("u4"));
    assert!(position("u4") < position("cp_4"));
}

/// The uncompacted case must be unchanged by the end-anchoring: each turn
/// still closes with the checkpoint taken inside it.
#[test]
fn hydrate_history_maps_checkpoints_to_turns_without_compaction() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "sys");
    for index in 0..3u64 {
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user(format!("u{index}"))));
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::assistant(
                format!("a{index}"),
                None,
                Vec::new(),
            )));
        let mut turn = deep_code_agent::TurnRecord::new();
        turn.started_at_ms = 10 * (index + 1);
        record.turns.push(turn);
        let mut checkpoint = deep_code_agent::CheckpointRecord::new(
            deep_code_agent::CheckpointId(format!("cp_{index}")),
            "before_turn",
        );
        checkpoint.created_at_ms = 10 * (index + 1) + 1;
        record.checkpoints.push(checkpoint);
    }

    let ids: Vec<String> = hydrate_history(&record)
        .into_iter()
        .filter_map(|cell| match cell {
            HistoryCell::Checkpoint { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["cp_0", "cp_1", "cp_2"]);
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
        HistoryCell::Reasoning { text, expanded } if text == "thinking" && !expanded
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

/// The whole "what may be hidden" policy, in one table.
///
/// The interesting cases are the refusals: a failure, a call a human answered,
/// and every tool that writes or that a user watches. The last row is the one
/// that matters most — a tool nobody classified must default to VISIBLE, so
/// adding a tool to the registry can never silently hide its calls.
#[test]
fn only_quiet_successful_calls_with_a_verb_may_fold() {
    use ToolApprovalState::{Approved, NotRequired, Required};
    use deep_code_agent::ToolResultStatus::{Denied, Error, Success};

    let folds =
        |tool: &str, approval, status| folded_entry(tool, "{}", approval, &status, "ok").is_some();

    for tool in [
        "read_file",
        "list_dir",
        "grep_files",
        "shell",
        "web_search",
        "fetch_url",
    ] {
        assert!(
            folds(tool, NotRequired, Success),
            "{tool} is noise and should fold"
        );
    }

    assert!(
        !folds("read_file", NotRequired, Error),
        "a failure must stay visible"
    );
    assert!(!folds("read_file", NotRequired, Denied));
    assert!(
        !folds("read_file", Approved, Success),
        "a human's decision is not noise"
    );
    assert!(!folds("read_file", Required, Success));

    for tool in [
        // Change the workspace: the transcript is the audit trail for exactly
        // these.
        "write_file",
        "apply_patch",
        "request_write_root",
        // Long-lived things a reader watches.
        "job",
        "agent",
        // Test doubles and — above all — anything nobody classified yet.
        "mock_echo",
        "brand_new_tool",
    ] {
        assert!(
            !folds(tool, NotRequired, Success),
            "{tool} must never be folded into a summary"
        );
    }
}

/// Nothing that folds may be something the live preview draws.
///
/// `folded_entry` is defined as `quiet_while_running` *minus the outcome test*,
/// which is a relationship two functions can drift out of in silence: widening
/// `quiet_while_running` to admit, say, a call a human answered would leave both
/// compiled and both plausible while a run hid a decision. So assert the
/// implication over the whole cross-product rather than trusting the comment.
///
/// Scope, so nobody trusts it further than it goes: this is a guardrail against
/// a *refactor* of those two functions, not independent verification of the
/// policy — `folded_entry` calls `quiet_while_running`, so the implication holds
/// by construction until someone changes that. The policy itself is pinned from
/// the outside by the agent-side records (`the_record_says_when_a_human_was_asked`)
/// and the render tests. And the tool list below is hand-written, so a tool
/// newly added to `tool_log_label` is not covered here until it is listed.
#[test]
fn nothing_that_folds_is_something_the_preview_draws() {
    use ToolApprovalState::{Approved, Denied as Refused, NotRequired, Required};
    use deep_code_agent::ToolResultStatus::{Denied, Error, Success};

    let tools = [
        // Foldable verbs, plus tools that must never fold at all.
        "read_file",
        "list_dir",
        "grep_files",
        "shell",
        "web_search",
        "fetch_url",
        "write_file",
        "apply_patch",
        "job",
        "agent",
        "request_write_root",
        "mock_echo",
        "brand_new_tool",
    ];
    for tool in tools {
        for approval in [NotRequired, Required, Approved, Refused] {
            for status in [Success, Error, Denied] {
                if folded_entry(tool, "{}", approval, &status, "ok").is_some() {
                    assert!(
                        quiet_while_running(tool, approval),
                        "{tool}/{approval:?}/{status:?} folds, so the live preview must \
                         have drawn nothing for it"
                    );
                }
            }
        }
    }
}

/// Resume rebuilds the same runs the live path builds, from the same predicate.
#[test]
fn hydrate_folds_a_run_of_quiet_calls_and_keeps_the_rest_visible() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("go")));
    let done = |content: &str| {
        Some(ExchangeResult {
            content: content.to_string(),
            status: deep_code_agent::ToolResultStatus::Success,
        })
    };
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "done",
            None,
            vec![
                ToolExchange {
                    call: call("c1", "read_file"),
                    result: done("a"),
                },
                ToolExchange {
                    call: call("c2", "read_file"),
                    result: done("b"),
                },
                // A write ends the run: it is never folded, and the reads
                // either side of it become two nodes rather than one that
                // would have to mention a call it is not showing.
                ToolExchange {
                    call: call("c3", "write_file"),
                    result: done("w"),
                },
                // A `shell` can be gated, and the record does not say whether
                // this one was: resume may not hide it behind a count.
                ToolExchange {
                    call: call("c3b", "shell"),
                    result: done("ran"),
                },
                ToolExchange {
                    call: call("c4", "read_file"),
                    result: done("c"),
                },
                // Interrupted before a result: no outcome to summarise.
                ToolExchange {
                    call: call("c5", "read_file"),
                    result: None,
                },
            ],
        )));

    let cells = hydrate_history(&record);
    let batches: Vec<usize> = cells
        .iter()
        .filter_map(|cell| match cell {
            HistoryCell::ToolBatch { entries, expanded } => {
                assert!(!expanded, "a resumed batch starts folded");
                Some(entries.len())
            }
            _ => None,
        })
        .collect();
    assert_eq!(batches, vec![2, 1], "got {cells:?}");
    assert!(
        cells.iter().any(|cell| matches!(
            cell,
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "shell"
        )),
        "a call that COULD have been gated stays visible on resume: {cells:?}"
    );
    assert!(
        cells
            .iter()
            .any(|cell| matches!(cell, HistoryCell::ToolCall { tool_name, .. } if tool_name == "write_file")),
        "the write keeps its own visible cell"
    );
    assert!(
        cells
            .iter()
            .any(|cell| matches!(cell, HistoryCell::ToolCall { tool_name, .. } if tool_name == "read_file")),
        "a call with no result stays visible rather than being summarised"
    );
}

/// `/copy` hands over what ran, not the folded summary — the same rule as
/// reasoning.
#[test]
fn copying_a_batch_yields_its_calls_and_results_not_its_summary() {
    let batch = HistoryCell::ToolBatch {
        entries: vec![ToolBatchEntry {
            tool_name: "read_file".to_string(),
            arguments: "{\"path\":\"a.rs\"}".to_string(),
            summary: "a.rs (12 lines)".to_string(),
        }],
        expanded: false,
    };
    let text = batch.lines(Lang::Zh).join("\n");
    assert!(text.contains("read_file"), "{text}");
    assert!(text.contains("a.rs (12 lines)"), "{text}");
}

#[test]
fn tool_call_renders_compact_single_line() {
    let tool = HistoryCell::ToolCall {
        tool_name: "shell".to_string(),
        arguments: "{\"command\":\n  \"grep foo\"}".to_string(),
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
        approval: ToolApprovalState::NotRequired,
        running_for_secs: Some(47),
    };
    assert!(running.lines(Lang::Zh)[0].contains("· 47s"));
}

#[test]
fn tool_result_renders_compact_single_line() {
    let result = HistoryCell::ToolResult {
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
