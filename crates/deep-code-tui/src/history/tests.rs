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
    let turn_id = TurnId::new();
    record.entries.push(std::sync::Arc::new(
        SessionEntry::user("hi").with_turn_id(turn_id.clone()),
    ));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: call("call_1", "mock_echo"),
                result: Some(ExchangeResult {
                    content: "mock_echo: hi".into(),
                    details: None,
                    status: ToolResultStatus::Denied,
                    asked: false,
                }),
            }],
        )));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::compaction(
            "older conversation summary",
            2,
        )));
    let mut turn = deep_code_agent::TurnRecord::new(turn_id.clone());
    turn.started_at_ms = 10;
    record.turns.push(turn);
    let mut checkpoint = deep_code_agent::CheckpointRecord::new(
        deep_code_agent::CheckpointId("checkpoint_1".to_string()),
        turn_id,
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
            ..
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
        expanded: true,
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
        expanded: true,
    };
    let lines = replayed.lines(Lang::En);
    assert!(lines[0].contains("2 record"), "{}", lines[0]);
    assert!(
        !lines[0].contains("tokens"),
        "no numbers, no token delta: {}",
        lines[0]
    );
}

/// Compaction removes entries without changing the surviving turn identities.
#[test]
fn hydrate_history_maps_checkpoints_to_turns_after_compaction() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "sys");
    // A session that ran five turns and was then compacted: only the last two
    // turns' entries survive, behind the compaction banner.
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::compaction("older", 6)));
    for index in 3..5 {
        record.entries.push(std::sync::Arc::new(
            SessionEntry::user(format!("u{index}")).with_turn_id(TurnId(format!("turn_{index}"))),
        ));
        record.entries.push(std::sync::Arc::new(
            SessionEntry::assistant(format!("a{index}"), None, Vec::new())
                .with_turn_id(TurnId(format!("turn_{index}"))),
        ));
    }
    for index in 0..5u64 {
        let turn_id = TurnId(format!("turn_{index}"));
        let mut turn = deep_code_agent::TurnRecord::new(turn_id.clone());
        turn.started_at_ms = 10 * (index + 1);
        record.turns.push(turn);
        let mut checkpoint = deep_code_agent::CheckpointRecord::new(
            deep_code_agent::CheckpointId(format!("cp_{index}")),
            turn_id,
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

/// Each turn closes with its own checkpoint, regardless of clock precision.
#[test]
fn hydrate_history_maps_checkpoints_to_turns_without_compaction() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "sys");
    for index in 0..3u64 {
        let turn_id = TurnId::new();
        record.entries.push(std::sync::Arc::new(
            SessionEntry::user(format!("u{index}")).with_turn_id(turn_id.clone()),
        ));
        record.entries.push(std::sync::Arc::new(
            SessionEntry::assistant(format!("a{index}"), None, Vec::new())
                .with_turn_id(turn_id.clone()),
        ));
        let mut turn = deep_code_agent::TurnRecord::new(turn_id.clone());
        turn.started_at_ms = 10;
        record.turns.push(turn);
        let mut checkpoint = deep_code_agent::CheckpointRecord::new(
            deep_code_agent::CheckpointId(format!("cp_{index}")),
            turn_id,
            "before_turn",
        );
        checkpoint.created_at_ms = 10;
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
fn hydrate_history_keeps_steering_with_its_turn_after_resume() {
    use deep_code_agent::{CheckpointId, CheckpointRecord, TurnRecord};
    use std::sync::Arc;

    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "sys");
    let first = TurnId::new();
    let second = TurnId::new();
    record.entries.extend([
        Arc::new(SessionEntry::user("first task").with_turn_id(first.clone())),
        Arc::new(SessionEntry::assistant("working", None, Vec::new()).with_turn_id(first.clone())),
        Arc::new(SessionEntry::user("steer first task").with_turn_id(first.clone())),
        Arc::new(
            SessionEntry::assistant("first done", None, Vec::new()).with_turn_id(first.clone()),
        ),
        Arc::new(SessionEntry::user("second task").with_turn_id(second.clone())),
        Arc::new(
            SessionEntry::assistant("second done", None, Vec::new()).with_turn_id(second.clone()),
        ),
    ]);
    // Equal timestamps cannot distinguish turns. The second turn is still
    // open at shutdown, so it has no completed TurnRecord yet.
    let mut first_record = TurnRecord::new(first.clone());
    first_record.started_at_ms = 100;
    record.turns.push(first_record);
    for (name, turn_id) in [("cp_first", first), ("cp_second", second)] {
        let mut checkpoint =
            CheckpointRecord::new(CheckpointId(name.into()), turn_id, "before_turn");
        checkpoint.created_at_ms = 100;
        record.checkpoints.push(checkpoint);
    }
    let resumed: SessionRecord =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    let rendered: Vec<String> = hydrate_history(&resumed)
        .into_iter()
        .filter_map(|cell| match cell {
            HistoryCell::User { text } | HistoryCell::Assistant { text } => Some(text),
            HistoryCell::Checkpoint { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        rendered,
        [
            "first task",
            "working",
            "steer first task",
            "first done",
            "cp_first",
            "second task",
            "second done",
            "cp_second",
        ]
    );
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
    use ToolApprovalState::{Approved, Denied as Refused, NotRequired, Required, Unknown};
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
        for approval in [NotRequired, Required, Approved, Refused, Unknown] {
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
            content: content.into(),
            details: None,
            status: deep_code_agent::ToolResultStatus::Success,
            asked: false,
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
    // `asked: false` on every record: nothing was granted, so the shell folds
    // exactly as it did live — which is the whole point of recording it.
    assert_eq!(batches, vec![2, 2], "got {cells:?}");
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
    assert!(
        !cells.iter().any(|cell| matches!(
            cell,
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "shell"
        )),
        "an un-granted shell folds on resume because it folded live: {cells:?}"
    );
}

/// Recorded approval decisions come back exactly as they were lived: the badge is restored, a call a human was asked about stays out of a
/// folded run, and a call nobody was asked about folds as it did live — which is
/// the standing-consent case (`auto_allow`, Yolo, a remembered command).
#[test]
fn hydrate_restores_what_the_record_kept() {
    let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
    let result = |content: &str, asked: bool| {
        Some(ExchangeResult {
            content: content.into(),
            details: None,
            status: deep_code_agent::ToolResultStatus::Success,
            asked,
        })
    };
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("go")));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "done",
            None,
            vec![
                // A human answered this one: it never folds, and it says so.
                ToolExchange {
                    call: call("c1", "shell"),
                    result: result("ran", true),
                },
                // Nobody was asked — a standing consent resolved the gate, or
                // the policy never raised it. The live view folds it, so this
                // does too.
                ToolExchange {
                    call: call("c2", "shell"),
                    result: result("ran", false),
                },
            ],
        )));

    let cells = hydrate_history(&record);
    let approved = cells
        .iter()
        .find_map(|cell| match cell {
            HistoryCell::ToolCall {
                tool_name,
                approval,
                ..
            } if tool_name == "shell" => Some(*approval),
            _ => None,
        })
        .expect("the answered call keeps a visible cell");
    assert_eq!(approved, ToolApprovalState::Approved);
    assert_eq!(
        approved.label(Lang::En),
        tr(Lang::En, TextId::BadgeApproved),
        "the badge the live view showed comes back"
    );
    let batch_sizes: Vec<usize> = cells
        .iter()
        .filter_map(|cell| match cell {
            HistoryCell::ToolBatch { entries, .. } => Some(entries.len()),
            _ => None,
        })
        .collect();
    assert_eq!(
        batch_sizes,
        vec![1],
        "the un-asked shell folds; the answered one does not: {cells:?}"
    );
}

/// A refusal a human made comes back as a badge; a refusal the policy made
/// without asking anyone does not — because the live view never badged it, and
/// the record now distinguishes the two.
///
/// `PolicyVerdict::Deny` refuses outright and deliberately never reaches a mode
/// or a consent, so a hard denial never puts anything in front of a human; a
/// parked refusal does. Both record `Denied`, so `asked` is what tells them
/// apart — which is why it, and not the status alone, drives this.
#[test]
fn hydrate_badges_a_refusal_only_when_a_human_made_it() {
    let exchange = |asked: bool| ToolExchange {
        call: call("c1", "shell"),
        result: Some(ExchangeResult {
            content: "declined".into(),
            details: None,
            status: deep_code_agent::ToolResultStatus::Denied,
            asked,
        }),
    };
    let badge_for = |asked: bool| {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "");
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::user("go")));
        record
            .entries
            .push(std::sync::Arc::new(SessionEntry::assistant(
                "done",
                None,
                vec![exchange(asked)],
            )));
        hydrate_history(&record)
            .iter()
            .find_map(|cell| match cell {
                HistoryCell::ToolCall { approval, .. } => Some(*approval),
                _ => None,
            })
            .expect("a denied call stays visible")
    };

    let human = badge_for(true);
    assert_eq!(human, ToolApprovalState::Denied);
    assert_eq!(human.label(Lang::Zh), tr(Lang::Zh, TextId::BadgeDenied));

    let hard = badge_for(false);
    assert_eq!(
        hard,
        ToolApprovalState::NotRequired,
        "the policy refused without asking, so there is no decision to badge"
    );
    assert_eq!(hard.label(Lang::Zh), "", "and it renders no badge");
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

/// The "Rules" value lists every loaded file with the lines actually kept,
/// global first, and says when the cap cut something.
#[test]
fn instructions_summary_lists_sources_and_truncation() {
    use deep_code_agent::{InstructionScope, InstructionSource, ProjectInstructions};
    let mut instructions = ProjectInstructions::default();
    assert_eq!(super::instructions_summary(Lang::Zh, &instructions), None);
    instructions.sources = vec![
        InstructionSource {
            scope: InstructionScope::Global,
            path: "/home/x/.deep-code/AGENTS.md".into(),
            lines: 10,
        },
        InstructionSource {
            scope: InstructionScope::Workspace,
            path: "/ws/AGENTS.md".into(),
            lines: 42,
        },
    ];
    assert_eq!(
        super::instructions_summary(Lang::Zh, &instructions).as_deref(),
        Some("~/.deep-code/AGENTS.md（10 行） + AGENTS.md（42 行）")
    );
    instructions.truncated = true;
    assert_eq!(
        super::instructions_summary(Lang::En, &instructions).as_deref(),
        Some("~/.deep-code/AGENTS.md (10 lines) + AGENTS.md (42 lines) · truncated to 24000 chars")
    );
    instructions.compact_instructions = Some("- keep the vetoes".to_string());
    assert_eq!(
        super::instructions_summary(Lang::Zh, &instructions).as_deref(),
        Some(
            "~/.deep-code/AGENTS.md（10 行） + AGENTS.md（42 行） · 已截断至 24000 字符 · 含压缩说明"
        )
    );
}

#[test]
fn ordered_image_observations_replay_as_readable_identity_and_keep_details() {
    use deep_code_agent::{Content, ContentBlock, ImageAsset, ImageFormat};
    let image = ImageAsset {
        workspace: "/workspace".into(),
        digest: "a".repeat(64),
        format: ImageFormat::Png,
        bytes: 70,
    };
    let content = Content::new(vec![
        ContentBlock::Text {
            text: "before".into(),
        },
        ContentBlock::Image {
            image: image.clone(),
        },
        ContentBlock::Text {
            text: "after".into(),
        },
    ]);
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user(content.clone())));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: call("call", "write_file"),
                result: Some(ExchangeResult {
                    content,
                    status: ToolResultStatus::Success,
                    asked: false,
                    details: Some(serde_json::json!({ "edit": {
                        "path": "a.txt", "diff": "+hello\n", "added": 1, "removed": 0,
                        "truncated": false, "new_file": true
                    }})),
                }),
            }],
        )));
    let cells = hydrate_history(&record);
    let HistoryCell::User { text } = &cells[0] else {
        panic!("user observation missing");
    };
    assert!(text.find("before").unwrap() < text.find(&image.digest).unwrap());
    assert!(text.find(&image.digest).unwrap() < text.find("after").unwrap());
    assert!(cells.iter().any(|cell| matches!(cell, HistoryCell::ToolResult { summary, .. } if summary.contains(&image.digest))));
    assert!(cells.iter().any(|cell| matches!(cell, HistoryCell::Diff { path, diff, .. } if path == "a.txt" && diff == "+hello\n")));
    assert!(!text.contains("base64"));
}

#[test]
fn execution_summary_reads_nested_stream_state_and_reports_incomplete_capture() {
    let details = serde_json::json!({
        "job_id": "job_1", "status": "failed", "exit_code": 2,
        "command": "run-check", "input_error": "pipe closed",
        "stdout": { "text": "normal output", "inline_truncated": true,
            "omitted_bytes": 42, "capture_complete": true,
            "spill": { "path": ".deep-code/output/job.stdout", "capped": true } },
        "stderr": { "text": "failure output", "inline_truncated": false,
            "capture_complete": false, "read_error": "read failed" }
    });
    let summary = summarize_tool_observation(&"bounded model text".into(), Some(&details));
    for fact in [
        "job_1: failed exit=2",
        "stdin: pipe closed",
        "stdout truncated",
        "42 bytes omitted",
        "file contains head only",
        "stderr capture failed: read failed",
        "stdout: normal output",
        "stderr: failure output",
    ] {
        assert!(summary.contains(fact), "missing {fact}: {summary}");
    }
}

#[test]
fn bounded_edit_details_keep_the_storage_notice_visible() {
    let content = deep_code_agent::Content::from(r#"{"path":"min.js","replacements":1}"#);
    let mut details = serde_json::json!({
        "edit": { "path": "min.js", "diff": "-old\n+new\n", "added": 1,
            "removed": 1, "truncated": true, "new_file": false }
    });
    assert_eq!(
        summarize_tool_observation(&content, Some(&details)),
        "min.js: 1 replacements"
    );
    details["details_storage"] = serde_json::json!({
        "inline_truncated": true,
        "path": ".deep-code/spill/tool-results/edit-details.txt",
        "complete": true, "original_bytes": 65536, "error": null
    });
    let summary = summarize_tool_observation(&content, Some(&details));
    for fact in [
        "min.js: 1 replacements",
        "details preview truncated",
        "65536 bytes originally",
        "full details saved .deep-code/spill/tool-results/edit-details.txt",
    ] {
        assert!(summary.contains(fact), "missing {fact}: {summary}");
    }
    assert!(matches!(HistoryCell::edit_diff(Some(&details)),
        Some(HistoryCell::Diff { truncated: true, diff, .. }) if diff == "-old\n+new\n"));
}

#[test]
fn execution_summary_keeps_details_storage_notice_after_stream_facts() {
    let details = serde_json::json!({
        "job_id": "job_1", "status": "completed", "exit_code": 0,
        "command": "check",
        "stdout": { "text": "captured output", "inline_truncated": false,
            "capture_complete": true, "read_error": null },
        "stderr": { "text": "", "inline_truncated": false,
            "capture_complete": true, "read_error": null },
        "details_storage": { "inline_truncated": true,
            "path": ".deep-code/spill/tool-results/job-details.txt",
            "complete": true, "original_bytes": 100000, "error": null }
    });
    let summary = summarize_tool_observation(&"model text".into(), Some(&details));
    assert!(summary.contains("job_1: completed exit=0"), "{summary}");
    assert!(summary.contains("stdout: captured output"), "{summary}");
    assert!(summary.contains("details preview truncated"), "{summary}");
    assert!(
        summary.contains("full details saved .deep-code/spill/tool-results/job-details.txt"),
        "{summary}"
    );
    assert!(!summary.contains("capture failed"), "{summary}");
    assert!(!summary.contains("capture incomplete"), "{summary}");
}

#[test]
fn details_storage_notice_distinguishes_partial_and_unavailable_sources() {
    let content = "custom tool finished".into();
    let partial = serde_json::json!({ "details_storage": {
        "inline_truncated": true, "path": ".deep-code/spill/tool-results/partial.txt",
        "complete": false, "original_bytes": 100000000, "error": null
    }});
    let summary = summarize_tool_observation(&content, Some(&partial));
    assert!(summary.starts_with("custom tool finished; details preview truncated"));
    assert!(
        summary.contains("partial details saved .deep-code/spill/tool-results/partial.txt"),
        "{summary}"
    );
    assert!(!summary.contains("full details saved"), "{summary}");

    for reason in ["output workspace unavailable", "permission denied"] {
        let unavailable = serde_json::json!({ "details_storage": {
            "inline_truncated": true, "path": null, "complete": false,
            "original_bytes": 100000, "error": reason
        }});
        let summary = summarize_tool_observation(&content, Some(&unavailable));
        assert!(summary.contains("omitted details unavailable"), "{summary}");
        assert!(summary.contains(reason), "{summary}");
        assert!(!summary.contains("details saved"), "{summary}");
    }
    let no_source = serde_json::json!({ "details_storage": {
        "inline_truncated": true, "path": null, "complete": true,
        "original_bytes": 100000, "error": null
    }});
    let summary = summarize_tool_observation(&content, Some(&no_source));
    assert!(summary.contains("omitted details unavailable"), "{summary}");
    assert!(!summary.contains("full details saved"), "{summary}");
}

#[test]
fn bounded_details_storage_reference_reappears_when_a_session_is_resumed() {
    let source = ".deep-code/spill/tool-results/missing-details.txt";
    let details = serde_json::json!({
        "edit": { "path": "min.js", "diff": "-old\n+new\n", "added": 1,
            "removed": 1, "truncated": true, "new_file": false },
        "details_storage": { "inline_truncated": true, "path": source,
            "complete": true, "original_bytes": 65536, "error": null }
    });
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::user("edit")));
    record
        .entries
        .push(std::sync::Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: call("edit", "apply_patch"),
                result: Some(ExchangeResult {
                    content: "changed min.js".into(),
                    details: Some(details.clone()),
                    status: ToolResultStatus::Success,
                    asked: false,
                }),
            }],
        )));
    let resumed: SessionRecord =
        serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
    let cells = hydrate_history(&resumed);
    let live_summary = summarize_tool_observation(&"changed min.js".into(), Some(&details));
    assert!(cells.iter().any(|cell| matches!(cell,
        HistoryCell::ToolResult { summary, .. } if summary == &live_summary && summary.contains(source)
    )));
    assert!(cells.iter().any(|cell| matches!(cell,
        HistoryCell::Diff { path, diff, truncated: true, .. }
            if path == "min.js" && diff == "-old\n+new\n"
    )));
}

#[test]
fn image_request_omission_reappears_when_a_turn_is_resumed() {
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    let turn_id = TurnId("turn_images".into());
    record.entries.push(std::sync::Arc::new(
        SessionEntry::user("look").with_turn_id(turn_id.clone()),
    ));
    let mut turn = deep_code_agent::TurnRecord::new(turn_id);
    turn.image_requests.push(
        serde_json::from_value(serde_json::json!({
            "model": "text-only", "images": [{
                "message_index": 1, "block_index": 1, "call_id": "call",
                "image": ".deep-code/images/asset.png", "included": false,
                "reason": "image not sent: current model does not accept images"
            }]
        }))
        .unwrap(),
    );
    record.turns.push(turn);
    let cells = hydrate_history(&record);
    assert!(cells.iter().any(|cell| matches!(cell, HistoryCell::System { text } if text.contains("text-only") && text.contains("image not sent"))));
}

#[test]
fn image_report_retention_notice_includes_compacted_turns_and_keeps_recent_reasons() {
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    let mut archived = deep_code_agent::TurnRecord::new(TurnId("archived".into()));
    archived.image_reports_omitted = 2;
    let current_id = TurnId("current".into());
    record.entries.push(std::sync::Arc::new(
        SessionEntry::user("latest task").with_turn_id(current_id.clone()),
    ));
    let mut current = deep_code_agent::TurnRecord::new(current_id);
    current.image_reports_omitted = 3;
    current.image_requests.push(
        serde_json::from_value(serde_json::json!({
            "model": "text-only", "images": [{
                "observation_id": "current:0:1", "message_index": 1, "block_index": 1,
                "call_id": null, "image": "sha256:asset", "included": false,
                "reason": "recent image omitted: current model does not accept images"
            }]
        }))
        .unwrap(),
    );
    record.turns.extend([archived, current]);
    let resumed: SessionRecord =
        serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
    let cells = hydrate_history(&resumed);
    let notices: Vec<_> = cells
        .iter()
        .filter_map(|cell| match cell {
            HistoryCell::System { text } if text.contains("retention budget") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "retention has one session-wide notice");
    assert!(notices[0].contains("较早的 5 条图片观察报告"));
    assert!(notices[0].contains("5 earlier image observation reports omitted"));
    assert!(cells.iter().any(|cell| matches!(cell,
        HistoryCell::System { text } if text.contains("text-only") && text.contains("recent image omitted")
    )));
    assert!(cells.iter().any(|cell| matches!(cell,
        HistoryCell::User { text } if text == "latest task"
    )));
}

#[test]
fn zero_omitted_image_reports_add_no_retention_notice() {
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    let turn_id = TurnId("current".into());
    record.entries.push(std::sync::Arc::new(
        SessionEntry::user("latest task").with_turn_id(turn_id.clone()),
    ));
    record.turns.push(deep_code_agent::TurnRecord::new(turn_id));
    let cells = hydrate_history(&record);
    assert_eq!(cells.len(), 1);
    assert!(matches!(&cells[0], HistoryCell::User { text } if text == "latest task"));
}

#[test]
fn omitted_image_report_count_saturates_without_panicking() {
    let mut record = SessionRecord::new(PathBuf::from("/workspace"), "");
    for (id, omitted) in [("older", u64::MAX), ("recent", 1)] {
        let mut turn = deep_code_agent::TurnRecord::new(TurnId(id.into()));
        turn.image_reports_omitted = omitted;
        record.turns.push(turn);
    }
    let cells = hydrate_history(&record);
    assert_eq!(cells.len(), 1);
    assert!(matches!(&cells[0], HistoryCell::System { text }
        if text.contains(&format!("{} earlier image observation reports", u64::MAX))
    ));
}

#[test]
fn execution_page_summary_exposes_continuation_and_memory_eviction() {
    let details = serde_json::json!({
        "job_id": "job", "command": "seq 1 1500", "status": "completed", "exit_code": 0,
        "stdout": {"text": "", "capture_complete": true, "inline_truncated": true, "omitted_bytes": 0},
        "stderr": {"text": "", "capture_complete": true},
        "output_page": {"stream": "stdout", "text": "next page", "start_byte": 100,
            "end_byte": 200, "total_bytes": 300, "unavailable_before_byte": 100,
            "has_more": true, "next_start_byte": 200},
    });
    let summary = summarize_tool_observation(&"page".into(), Some(&details));
    assert!(summary.contains("stdout bytes 100..200 of 300"));
    assert!(summary.contains("bytes before 100 unavailable from memory"));
    assert!(summary.contains("next byte 200") && summary.contains("next page"));
    assert!(!summary.contains("truncated (0 bytes") && !summary.contains("capture incomplete"));
    let mut running = details.clone();
    running["status"] = "running".into();
    running["output_page"]["has_more"] = false.into();
    let summary = summarize_tool_observation(&"page".into(), Some(&running));
    assert!(summary.contains("waiting for output at byte 200"));
    running["output_page"]["next_start_byte"] = serde_json::Value::Null;
    let summary = summarize_tool_observation(&"page".into(), Some(&running));
    assert!(summary.contains("end of retained output"));
}

#[test]
fn text_page_summary_does_not_invent_an_unknown_total() {
    let summary = summarize_tool_result(
        &serde_json::json!({
            "path": "virtual.txt", "total_lines": null,
            "lines": [{"text": "one"}], "truncated": true,
        })
        .to_string(),
    );
    assert!(summary.contains("1 lines shown (total unknown, truncated=true)"));
}
