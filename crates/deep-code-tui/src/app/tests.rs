use super::*;

#[test]
fn slash_help_clear_and_status_update_history() {
    let mut app = App::new();

    assert!(app.handle_slash_command("/help"));
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains("/status")
    ));

    assert!(app.handle_slash_command("/status"));
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains("backend=")
    ));

    // `/clear` starts a fresh conversation: the transcript resets to just
    // the welcome header rather than going empty.
    assert!(app.handle_slash_command("/clear"));
    assert!(
        matches!(app.history.as_slice(), [HistoryCell::Welcome { .. }]),
        "clear leaves only the welcome header, got {} cells",
        app.history.len()
    );
    assert!(app.status.contains("新对话"));
}

#[test]
fn resume_picker_navigates_and_cancels() {
    use deep_code_agent::{AgentConfig, SessionEntry};
    let make = |prompt: &str| {
        let mut record = SessionRecord::new(
            std::path::PathBuf::from("/tmp/ws"),
            &AgentConfig::builtin(),
            "system",
        );
        record.entries = vec![SessionEntry::system("system"), SessionEntry::user(prompt)];
        record
    };
    let mut app = App::new();
    app.resume_picker = Some(ResumePicker {
        sessions: vec![make("a"), make("b"), make("c")],
        selected: 0,
    });
    assert!(app.resume_picker_open());

    app.resume_picker_down();
    app.resume_picker_down();
    assert_eq!(app.resume_picker.as_ref().unwrap().selected, 2);
    app.resume_picker_down(); // clamp at last row
    assert_eq!(app.resume_picker.as_ref().unwrap().selected, 2);
    app.resume_picker_up();
    assert_eq!(app.resume_picker.as_ref().unwrap().selected, 1);

    app.resume_picker_cancel();
    assert!(app.resume_picker.is_none());
    assert!(app.status.contains("已取消"));
}

#[test]
fn slash_resume_blocked_while_streaming() {
    let mut app = App::new();
    app.is_streaming = true;
    assert!(app.handle_slash_command("/resume"));
    assert!(app.resume_picker.is_none());
    assert!(
        app.status.contains("无法切换会话"),
        "status: {}",
        app.status
    );
}

#[test]
fn resume_command_is_registered() {
    assert!(
        crate::commands::SLASH_COMMANDS
            .iter()
            .any(|(name, _, _)| *name == "/resume")
    );
}

#[test]
fn scroll_helpers_adjust_offset() {
    let mut app = App::new();
    app.scroll_up();
    assert_eq!(app.scroll_offset, 3);
    app.scroll_down();
    assert_eq!(app.scroll_offset, 0);
    app.scroll_up();
    app.scroll_to_bottom();
    assert_eq!(app.scroll_offset, 0);
}

#[test]
fn history_cap_drops_oldest_cells_and_counts_them() {
    let mut app = App::new();
    for index in 0..(MAX_HISTORY_CELLS + 100) {
        app.history
            .push(HistoryCell::system(format!("cell-{index}")));
    }
    app.enforce_history_cap();

    assert_eq!(app.history.len(), MAX_HISTORY_CELLS);
    assert!(app.trimmed_cells >= 100);
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.ends_with(&format!("cell-{}", MAX_HISTORY_CELLS + 99))
    ));
    // Oldest survivor is a late cell, not cell-0.
    assert!(matches!(
        app.history.first(),
        Some(HistoryCell::System { text }) if !text.ends_with("cell-0")
    ));
}

#[test]
fn find_in_transcript_jumps_and_continues_upward() {
    let mut app = App::new();
    let mut lines: Vec<String> = (0..50).map(|index| format!("line {index}")).collect();
    lines[10] = "needle alpha".to_string();
    lines[30] = "NEEDLE beta".to_string();
    app.transcript = Some(TranscriptSnapshot {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
        scroll_top: 0,
        lines,
    });

    // First /find: nearest-to-bottom match (line 30, case-insensitive).
    app.find_in_transcript("needle");
    assert_eq!(app.find_state.as_ref().unwrap().1, 30);
    // max_scroll = 50 - 10 = 40; match at top of viewport → offset 10.
    assert_eq!(app.scroll_offset, 10);

    // Same query again: continues upward to line 10.
    app.find_in_transcript("needle");
    assert_eq!(app.find_state.as_ref().unwrap().1, 10);
    assert_eq!(app.scroll_offset, 30);

    // Exhausted: resets so the next /find starts from the bottom again.
    app.find_in_transcript("needle");
    assert!(app.find_state.is_none());
    assert!(app.status.contains("最早匹配"));
    app.find_in_transcript("needle");
    assert_eq!(app.find_state.as_ref().unwrap().1, 30);

    // Unknown query reports not-found.
    app.find_in_transcript("missing-term");
    assert!(app.status.contains("未找到"));
}

#[test]
fn approval_scroll_helpers_adjust_panel_offset() {
    let mut app = App::new();
    app.pending_approval = Some(deep_code_agent::ApprovalRequest {
        call_id: "call_1".to_string(),
        tool_name: "write_file".to_string(),
        description: "Write a file".to_string(),
        arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
        risk_level: deep_code_agent::RiskLevel::High,
        requires_sandbox: true,
        read_only: false,
        matched_rule: Some("write".to_string()),
        preview: None,
        safety_reasons: Vec::new(),
        safety_suggestions: Vec::new(),
    });
    for _ in 0..10 {
        app.scroll_approval_down();
    }
    assert_eq!(
        app.approval_scroll_offset,
        app.clamped_approval_scroll_offset()
    );
    assert!(app.approval_scroll_offset > 0);
    app.scroll_approval_up();
    assert!(app.approval_scroll_offset < app.approval_scroll_max());
    app.scroll_approval_down();
    app.scroll_approval_to_top();
    assert_eq!(app.approval_scroll_offset, 0);
}

#[test]
fn status_includes_deepseek_native_telemetry() {
    let mut app = App::new();
    app.last_telemetry = Some(TurnTelemetry {
        route_label: "auto→deepseek-v4-flash (high)".to_string(),
        effective_model: "deepseek-v4-flash".to_string(),
        reasoning_effort: "high".to_string(),
        prompt_tokens: 100,
        completion_tokens: 20,
        cache_hit_tokens: Some(80),
        cache_miss_tokens: Some(20),
        session_cache_hit_tokens: 80,
        session_cache_miss_tokens: 20,
        session_cache_savings: deep_code_agent::CostEstimate::default(),
        prefix_status: deep_code_agent::PrefixStatus::Stable,
        route_reason: "短提示优先使用 Flash".to_string(),
        route_source: "heuristic".to_string(),
        cascade_triggered: false,
        fallback_reason: None,
        context_window: 1_000_000,
        estimated_context_tokens: 120,
        context_usage_percent: 1,
        near_compaction_threshold: false,
        used_model_fallback: false,
        stream_retries: 2,
        turn_cost: deep_code_agent::CostEstimate {
            cny: 0.001,
            usd: 0.0001,
        },
        session_cost: deep_code_agent::CostEstimate {
            cny: 0.002,
            usd: 0.0002,
        },
    });

    assert!(app.handle_slash_command("/status"));
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text })
            if text.contains("effective_model=deepseek-v4-flash")
                && text.contains("auto_reason=短提示优先使用 Flash")
                && text.contains("session_cost=¥0.0020")
                && text.contains("stream_retries=2")
    ));
}

#[test]
fn tool_finished_flushes_tool_call_before_result_without_duplicate() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "please echo".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        tool_name: "mock_echo".to_string(),
        arguments: serde_json::json!({ "message": "hi" }),
    });

    let result = deep_code_agent::ToolResult::success("call_1", "mock_echo", "mock_echo: hi");
    app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
        turn_id: Some(turn_id),
        tool_call_id,
        result,
    });

    let tool_call_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
        .expect("tool call cell");
    let tool_result_indices = app
        .history
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| matches!(cell, HistoryCell::ToolResult { .. }).then_some(index))
        .collect::<Vec<_>>();

    assert_eq!(tool_result_indices.len(), 1);
    assert!(tool_call_index < tool_result_indices[0]);
}

#[test]
fn multi_tool_cells_flush_independently_per_finished_call() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    let call_1 = deep_code_agent::ToolCallId("call_1".to_string());
    let call_2 = deep_code_agent::ToolCallId("call_2".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "run both".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
        turn_id: turn_id.clone(),
        tool_call_id: call_1.clone(),
        tool_name: "git_echo".to_string(),
        arguments: serde_json::json!({ "message": "one" }),
    });
    app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
        turn_id: turn_id.clone(),
        tool_call_id: call_2.clone(),
        tool_name: "mock_echo".to_string(),
        arguments: serde_json::json!({ "message": "two" }),
    });

    app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
        turn_id: Some(turn_id.clone()),
        tool_call_id: call_1,
        result: deep_code_agent::ToolResult::success("call_1", "git_echo", "git_echo: one"),
    });

    // call_1 cell flushed to history; call_2 still streaming in active turn.
    assert!(app.history.iter().any(|cell| matches!(
        cell,
        HistoryCell::ToolCall { tool_name, .. } if tool_name == "git_echo"
    )));
    assert!(app.history.iter().all(|cell| !matches!(
        cell,
        HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
    )));
    let active = app.active_turn.as_ref().expect("active turn kept");
    assert_eq!(active.tools.len(), 1);
    assert_eq!(active.tools[0].tool_name, "mock_echo");

    app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
        turn_id: Some(turn_id),
        tool_call_id: call_2,
        result: deep_code_agent::ToolResult::success("call_2", "mock_echo", "mock_echo: two"),
    });

    let tool_cells = app
        .history
        .iter()
        .filter_map(|cell| match cell {
            HistoryCell::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_cells, vec!["git_echo", "mock_echo"]);
    let result_cells = app
        .history
        .iter()
        .filter(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
        .count();
    assert_eq!(result_cells, 2);
    assert!(
        app.active_turn
            .as_ref()
            .is_some_and(|active| active.tools.is_empty())
    );
}

#[test]
fn composer_newline_and_height_clamp() {
    let mut app = App::new();
    assert_eq!(
        app.input_height(80),
        3,
        "empty input is one row plus borders"
    );

    app.push_char('a');
    app.push_newline();
    app.push_char('b');
    assert_eq!(app.input, "a\nb");
    assert_eq!(app.input_height(80), 4);

    for _ in 0..10 {
        app.push_newline();
    }
    assert_eq!(
        app.input_height(80),
        8,
        "content rows clamp at 6 (+2 borders)"
    );

    app.is_streaming = true;
    let before = app.input.clone();
    app.push_newline();
    assert_eq!(app.input, before, "no edits while streaming");
}

#[test]
fn prompt_history_navigates_and_preserves_draft() {
    let mut app = App::new();
    app.remember_prompt("first");
    app.remember_prompt("second");
    app.remember_prompt("second");
    assert_eq!(
        app.prompt_history,
        vec!["first".to_string(), "second".to_string()],
        "consecutive duplicates collapse"
    );

    app.input = "draft".to_string();
    app.history_prev();
    assert_eq!(app.input, "second");
    app.history_prev();
    assert_eq!(app.input, "first");
    app.history_prev();
    assert_eq!(app.input, "first", "clamped at oldest");

    app.history_next();
    assert_eq!(app.input, "second");
    app.history_next();
    assert_eq!(app.input, "draft", "draft restored past newest");

    // Editing a recalled entry detaches from history without mutating it.
    app.history_prev();
    assert_eq!(app.input, "second");
    app.push_char('!');
    assert_eq!(app.input, "second!");
    assert_eq!(app.prompt_history[1], "second");
    app.history_prev();
    assert_eq!(
        app.input, "second",
        "after edits, Ctrl+P starts a fresh walk"
    );
}

#[test]
fn prompt_history_caps_at_limit() {
    let mut app = App::new();
    for index in 0..150 {
        app.remember_prompt(&format!("prompt-{index}"));
    }
    assert_eq!(app.prompt_history.len(), 100);
    assert_eq!(app.prompt_history[0], "prompt-50");
    assert_eq!(app.prompt_history[99], "prompt-149");
}

#[test]
fn apikey_command_validates_writes_and_stays_out_of_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.global_config_path = dir.path().join("config.toml");

    // Invalid key: rejected, nothing written, nothing remembered.
    app.input = "/apikey short".to_string();
    app.submit();
    assert!(!app.global_config_path.exists());
    assert!(app.prompt_history.is_empty());
    assert!(app.status.contains("API key") || app.status.contains("长度"));

    // Valid key: persisted, runtime relaunched onto DeepSeek, and the
    // key is recallable nowhere (history, prompts, status).
    app.input = "/apikey sk-0123456789abcdef".to_string();
    app.submit();
    let contents = std::fs::read_to_string(&app.global_config_path).unwrap();
    assert!(contents.contains("sk-0123456789abcdef"));
    assert!(
        app.prompt_history.is_empty(),
        "key must not enter Ctrl+P history"
    );
    assert!(app.backend_label.contains("DeepSeek"));
    assert!(!app.status.contains("sk-0123456789abcdef"));
    assert!(
        app.history
            .iter()
            .all(|cell| !format!("{cell:?}").contains("sk-0123456789abcdef")),
        "key must not appear in any transcript cell"
    );

    // Ordinary slash commands ARE remembered (contrast).
    app.input = "/help".to_string();
    app.submit();
    assert_eq!(app.prompt_history, vec!["/help".to_string()]);
}

#[test]
fn model_command_resolves_aliases_persists_and_keeps_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.global_config_path = dir.path().join("config.toml");
    let original_session = app.session_id.clone();
    assert!(
        original_session.is_some(),
        "test relies on persisted session"
    );

    assert!(app.handle_slash_command("/model flash"));
    assert_eq!(app.configured_model, deep_code_agent::DEEPSEEK_V4_FLASH);
    let contents = std::fs::read_to_string(&app.global_config_path).unwrap();
    assert!(contents.contains("deepseek-v4-flash"));
    assert_eq!(
        app.session_id, original_session,
        "config switch must resume the same session"
    );

    assert!(app.handle_slash_command("/model nope"));
    assert!(app.status.contains("未知模型"));
    assert!(app.status.contains("auto"));

    assert!(app.handle_slash_command("/model"));
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains("用法")
    ));
}

#[test]
fn logout_removes_key_from_global_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.global_config_path = dir.path().join("config.toml");

    assert!(app.handle_slash_command("/apikey sk-0123456789abcdef"));
    assert!(
        std::fs::read_to_string(&app.global_config_path)
            .unwrap()
            .contains("api_key")
    );

    assert!(app.handle_slash_command("/logout"));
    assert!(
        !std::fs::read_to_string(&app.global_config_path)
            .unwrap()
            .contains("api_key")
    );
}

#[test]
fn slash_menu_filters_navigates_and_completes() {
    let mut app = App::new();
    app.push_char('/');
    assert!(app.completion_open(), "typing '/' opens the command menu");
    app.push_char('h');
    app.push_char('e');
    let menu = app.completion.as_ref().expect("menu open");
    assert_eq!(menu.kind, CompletionKind::Slash);
    assert_eq!(menu.items.len(), 1);
    assert_eq!(menu.items[0].0, "/help");

    let ready = app.accept_completion();
    assert!(ready, "argless command is ready to run");
    assert_eq!(app.input, "/help");
    assert!(!app.completion_open());

    // Argument-taking command completes with a trailing space, not ready.
    app.input.clear();
    app.push_char('/');
    app.push_char('r');
    app.push_char('e');
    let ready = app.accept_completion();
    assert!(!ready);
    assert_eq!(app.input, "/restore ");

    // After a space the slash menu stays closed.
    app.push_char('x');
    assert!(!app.completion_open());
}

#[test]
fn file_menu_completes_trailing_at_token() {
    let mut app = App::new();
    app.workspace_files = vec![
        "Cargo.toml".to_string(),
        "src/main.rs".to_string(),
        "src/markdown.rs".to_string(),
    ];
    for ch in "see @ma".chars() {
        app.push_char(ch);
    }
    let menu = app.completion.as_ref().expect("file menu open");
    assert_eq!(menu.kind, CompletionKind::File);
    assert!(menu.items.iter().any(|(value, _)| value == "src/main.rs"));

    app.completion_down();
    app.completion_up();
    let ready = app.accept_completion();
    assert!(!ready);
    assert!(app.input.starts_with("see src/ma"), "input: {}", app.input);
    assert!(app.input.ends_with(' '));
    assert!(!app.input.contains('@'), "@ marker is stripped");

    // An email-like token does not start with '@' → no menu.
    app.input.clear();
    for ch in "mail a@b".chars() {
        app.push_char(ch);
    }
    assert!(!app.completion_open());
}

#[test]
fn escape_closes_completion_before_clearing_input() {
    let mut app = App::new();
    app.push_char('/');
    app.push_char('h');
    assert!(app.completion_open());

    app.handle_escape();
    assert!(!app.completion_open(), "first Esc closes the menu");
    assert_eq!(app.input, "/h", "input preserved");

    app.handle_escape();
    assert!(app.input.is_empty(), "second Esc clears input");
    assert!(!app.should_quit);
}

#[tokio::test]
async fn approval_a_key_resolves_for_session() {
    let mut app = App::new();
    app.pending_approval = Some(deep_code_agent::ApprovalRequest {
        call_id: "call_1".to_string(),
        tool_name: "mock_echo".to_string(),
        description: "echo".to_string(),
        arguments: serde_json::json!({}),
        risk_level: deep_code_agent::RiskLevel::Low,
        requires_sandbox: false,
        read_only: true,
        matched_rule: None,
        preview: None,
        safety_reasons: Vec::new(),
        safety_suggestions: Vec::new(),
    });

    app.approve_pending_tool_for_session();

    assert!(app.pending_approval.is_none());
    assert!(app.is_streaming);
    assert!(app.status.contains("approved (session)"));
}

#[test]
fn multiline_paste_collapses_to_placeholder_and_expands_on_send() {
    let mut app = App::new();
    for ch in "看这个 ".chars() {
        app.push_char(ch);
    }
    app.paste_str("line1\nline2\nline3".to_string());

    // Composer shows a compact chip, not the raw content.
    assert!(app.input.contains("[粘贴 #1 +3 行]"));
    assert!(!app.input.contains("line2"));
    assert_eq!(app.pasted_blocks.len(), 1);

    // Sent form expands the chip back to the real content.
    let sent = app.expand_pasted(&app.input);
    assert!(sent.contains("line1\nline2\nline3"));
    assert!(!sent.contains("[粘贴"));
}

#[test]
fn short_single_line_paste_stays_inline() {
    let mut app = App::new();
    app.paste_str("quick".to_string());
    assert_eq!(app.input, "quick");
    assert!(app.pasted_blocks.is_empty());
}

#[test]
fn long_single_line_paste_collapses_with_char_count() {
    let mut app = App::new();
    app.paste_str("x".repeat(200));
    assert!(app.input.contains("[粘贴 #1 · 200 字]"));
    assert_eq!(app.pasted_blocks.len(), 1);
}

#[test]
fn ctrl_w_deletes_previous_word() {
    let mut app = App::new();
    for ch in "hello world".chars() {
        app.push_char(ch);
    }
    app.delete_word_back();
    assert_eq!(app.input, "hello ");
    assert_eq!(app.input_cursor, 6);
    app.delete_word_back();
    assert_eq!(app.input, "");
}

#[test]
fn ctrl_u_and_ctrl_k_kill_line_segments() {
    let mut app = App::new();
    for ch in "abcdef".chars() {
        app.push_char(ch);
    }
    app.cursor_left();
    app.cursor_left();
    app.cursor_left(); // cursor at index 3 (between c and d)
    app.kill_to_line_end();
    assert_eq!(app.input, "abc");
    app.kill_to_line_start();
    assert_eq!(app.input, "");
}

#[test]
fn word_movement_jumps_between_words() {
    let mut app = App::new();
    for ch in "foo bar baz".chars() {
        app.push_char(ch);
    }
    assert_eq!(app.input_cursor, 11);
    app.word_left();
    assert_eq!(app.input_cursor, 8); // start of "baz"
    app.word_left();
    assert_eq!(app.input_cursor, 4); // start of "bar"
    app.word_right();
    assert_eq!(app.input_cursor, 7); // end of "bar"
}

fn snapshot(lines: &[&str]) -> TranscriptSnapshot {
    TranscriptSnapshot {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
        scroll_top: 0,
        lines: lines.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn slice_by_display_cols_handles_cjk_boundaries() {
    assert_eq!(slice_by_display_cols("hello", 1, 4), "ell");
    // "你好世界" = 8 display cols; cols [2,6) spans 好世.
    assert_eq!(slice_by_display_cols("你好世界", 2, 6), "好世");
    // A CJK char straddling the boundary is kept whole.
    assert_eq!(slice_by_display_cols("你好", 1, 2), "你");
}

#[test]
fn drag_selection_extracts_multiline_text() {
    let mut app = App::new();
    // text origin x = snapshot.x + 1 = 1; row 0 = line 0.
    app.set_transcript_snapshot(snapshot(&["first line", "second line", "third"]));

    // Drag from line 0 col 6 ("line") to line 1 col 7 ("second ").
    app.selection_begin(1 + 6, 0);
    app.selection_update(1 + 7, 1);
    let text = app.selection_finish().expect("non-empty selection");
    assert_eq!(text, "line\nsecond ");
}

#[test]
fn plain_click_makes_no_selection() {
    let mut app = App::new();
    app.set_transcript_snapshot(snapshot(&["abc"]));
    app.selection_begin(2, 0);
    app.selection_update(2, 0); // no movement
    assert!(app.selection_finish().is_none());
    assert!(app.selection.is_none());
}

#[test]
fn mouse_outside_transcript_clears_selection() {
    let mut app = App::new();
    app.set_transcript_snapshot(snapshot(&["abc"]));
    app.selection_begin(200, 200); // outside
    assert!(app.selection.is_none());
}

#[test]
fn arrows_drive_draft_and_history_never_scroll() {
    let mut app = App::new();
    app.remember_prompt("earlier");

    // Single-line draft: ↑ recalls history (stashing the draft), ↓ restores
    // it; the transcript scroll offset never changes.
    for ch in "typing".chars() {
        app.push_char(ch);
    }
    app.on_up();
    assert_eq!(app.scroll_offset, 0, "↑ never scrolls");
    assert_eq!(app.input, "earlier", "single-line ↑ recalls history");
    app.on_down();
    assert_eq!(app.input, "typing", "↓ restores the stashed draft");
    assert_eq!(app.scroll_offset, 0);

    // Multi-line draft: ↑ moves the cursor; at the top line it does nothing
    // (must not clobber the draft with history).
    app.input.clear();
    app.input_cursor = 0;
    for ch in "l1\nl2".chars() {
        app.push_char(ch);
    }
    app.on_up();
    assert_eq!(app.cursor_line_col().0, 0, "↑ moved cursor to first line");
    let at_top = app.input.clone();
    app.on_up();
    assert_eq!(app.input, at_top, "top-line ↑ must not recall history");
    assert_eq!(app.scroll_offset, 0);
}

#[test]
fn ctrl_c_requires_double_press_when_idle() {
    let mut app = App::new();
    app.handle_ctrl_c();
    assert!(!app.should_quit, "first Ctrl+C must not quit");
    assert!(app.ctrl_c_pending);
    assert!(app.status.contains("再按一次"));
    app.handle_ctrl_c();
    assert!(app.should_quit, "second consecutive Ctrl+C quits");
}

#[test]
fn ctrl_c_clears_input_before_quitting() {
    let mut app = App::new();
    for ch in "draft".chars() {
        app.push_char(ch);
    }
    app.handle_ctrl_c();
    assert!(app.input.is_empty());
    assert!(!app.should_quit);
    assert!(!app.ctrl_c_pending, "clearing input does not arm the guard");
}

#[test]
fn ctrl_c_guard_disarmed_by_other_key() {
    let mut app = App::new();
    app.handle_ctrl_c(); // arm
    assert!(app.ctrl_c_pending);
    app.clear_ctrl_c_guard(); // any other key disarms
    app.handle_ctrl_c(); // counts as a fresh first press
    assert!(!app.should_quit);
    assert!(app.ctrl_c_pending);
}

#[test]
fn ctrl_c_while_streaming_does_not_quit() {
    let mut app = App::new();
    app.is_streaming = true;
    app.handle_ctrl_c();
    assert!(!app.should_quit, "Ctrl+C interrupts the turn, not the app");
}

#[test]
fn streaming_activity_shows_only_while_streaming() {
    let mut app = App::new();
    assert!(
        app.streaming_activity().is_none(),
        "idle shows no indicator"
    );

    app.is_streaming = true;
    app.streaming_since = Some(std::time::Instant::now());
    let activity = app.streaming_activity().expect("indicator while streaming");
    assert!(activity.contains("生成中"));

    app.is_streaming = false;
    assert!(app.streaming_activity().is_none());
}

#[test]
fn escape_ladder_clears_input_then_quits() {
    let mut app = App::new();
    app.input = "draft".to_string();

    app.handle_escape();
    assert!(app.input.is_empty());
    assert!(!app.should_quit);
    assert!(app.status.contains("已清空输入"));

    app.handle_escape();
    assert!(app.should_quit);
}

#[test]
fn escape_during_streaming_requests_cancel_without_quitting() {
    let mut app = App::new();
    app.is_streaming = true;
    app.input = "keep me".to_string();

    app.handle_escape();

    assert!(!app.should_quit);
    assert_eq!(app.input, "keep me");
    assert!(app.status.contains("取消"));
}

#[test]
fn error_event_flushes_partial_assistant_before_error_cell() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "hi".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::AssistantDelta {
        turn_id: turn_id.clone(),
        text: "partial".to_string(),
    });

    app.apply_runtime_event(RuntimeEvent::Error {
        turn_id: Some(turn_id),
        message: "boom".to_string(),
    });

    assert!(app.active_turn.is_none(), "active turn flushed on error");
    let assistant_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::Assistant { text } if text == "partial"))
        .expect("partial assistant kept in history");
    let error_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::System { text } if text.contains("boom")))
        .expect("error cell");
    assert!(assistant_index < error_index);
}

#[test]
fn turn_cancelled_event_flushes_active_turn_and_resets_state() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "hang".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::AssistantDelta {
        turn_id: turn_id.clone(),
        text: "partial".to_string(),
    });
    app.is_streaming = true;

    app.apply_runtime_event(RuntimeEvent::TurnCancelled { turn_id });

    assert!(!app.is_streaming);
    assert!(app.status.contains("已取消"));
    assert!(app.active_turn.is_none());
    assert!(app.history.iter().any(|cell| matches!(
        cell,
        HistoryCell::Assistant { text } if text == "partial"
    )));
    assert!(app.history.iter().any(|cell| matches!(
        cell,
        HistoryCell::System { text } if text.contains("已取消")
    )));
}

#[test]
fn approval_events_render_pending_and_resolved_tool_metadata() {
    let mut app = App::new();
    app.scroll_up();
    app.scroll_approval_down();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "write something".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
    });
    app.apply_runtime_event(RuntimeEvent::ApprovalRequired {
        turn_id: Some(turn_id.clone()),
        tool_call_id: Some(tool_call_id.clone()),
        request: deep_code_agent::ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "write_file".to_string(),
            description: "Write note.txt".to_string(),
            arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
            risk_level: deep_code_agent::RiskLevel::High,
            requires_sandbox: true,
            read_only: false,
            matched_rule: Some("write".to_string()),
            preview: None,
            safety_reasons: Vec::new(),
            safety_suggestions: Vec::new(),
        },
    });

    let preview = app.active_turn.as_ref().unwrap().preview_cells();
    assert!(preview.iter().any(|cell| matches!(
        cell,
        HistoryCell::ToolCall {
            risk_level: Some(risk),
            requires_sandbox: Some(true),
            approval,
            ..
        } if risk == "High" && *approval == crate::history::ToolApprovalState::Required
    )));
    // The approval is surfaced by the dedicated panel (app.pending_approval),
    // not duplicated inline in the transcript preview.
    assert!(
        !preview
            .iter()
            .any(|cell| matches!(cell, HistoryCell::Approval { .. })),
        "approval must not be duplicated in the transcript preview"
    );
    let pending = app.pending_approval.as_ref().expect("pending approval set");
    assert_eq!(pending.matched_rule.as_deref(), Some("write"));
    assert_eq!(format!("{:?}", pending.risk_level), "High");
    assert!(pending.requires_sandbox);
    assert_eq!(app.scroll_offset, 3);
    assert_eq!(app.approval_scroll_offset, 0);

    app.pending_approval = None;
    app.apply_runtime_event(RuntimeEvent::ApprovalResolved {
        turn_id: Some(turn_id.clone()),
        tool_call_id: tool_call_id.clone(),
        decision: deep_code_agent::ApprovalDecision::Approved,
    });
    let result =
        deep_code_agent::ToolResult::success("call_1", "write_file", "{\"bytes_written\":5}");
    app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
        turn_id: Some(turn_id),
        tool_call_id,
        result,
    });
    assert!(app.history.iter().any(|cell| matches!(
        cell,
        HistoryCell::ToolCall { approval, .. }
            if *approval == crate::history::ToolApprovalState::Approved
    )));
}

#[test]
fn diagnostics_are_flushed_after_tool_call_before_result() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "edit file".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({ "path": "src/main.rs" }),
    });
    app.apply_runtime_event(RuntimeEvent::DiagnosticsUpdated {
        summary: "1 warning".to_string(),
        rendered: "warning: unused variable".to_string(),
    });
    assert!(
        app.history
            .iter()
            .all(|cell| !matches!(cell, HistoryCell::Diagnostics { .. }))
    );

    let result = deep_code_agent::ToolResult::success(
        "call_1",
        "write_file",
        "{\"path\":\"src/main.rs\",\"bytes_written\":10}",
    );
    app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
        turn_id: Some(turn_id),
        tool_call_id,
        result,
    });

    let tool_call_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
        .expect("tool call");
    let diagnostics_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::Diagnostics { .. }))
        .expect("diagnostics");
    let tool_result_index = app
        .history
        .iter()
        .position(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
        .expect("tool result");

    assert!(tool_call_index < diagnostics_index);
    assert!(diagnostics_index < tool_result_index);
}

#[test]
fn status_line_includes_mode_backend_session_checkpoint_and_cost() {
    let mut app = App::new();
    app.session_id = Some("session_1".to_string());
    app.last_checkpoint = Some("checkpoint_1".to_string());
    app.last_telemetry = Some(TurnTelemetry {
        route_label: "auto->deepseek-v4-flash (high)".to_string(),
        effective_model: "deepseek-v4-flash".to_string(),
        reasoning_effort: "high".to_string(),
        prompt_tokens: 100,
        completion_tokens: 20,
        cache_hit_tokens: Some(80),
        cache_miss_tokens: Some(20),
        session_cache_hit_tokens: 80,
        session_cache_miss_tokens: 20,
        session_cache_savings: deep_code_agent::CostEstimate::default(),
        prefix_status: deep_code_agent::PrefixStatus::Stable,
        route_reason: "short prompt".to_string(),
        route_source: "heuristic".to_string(),
        cascade_triggered: false,
        fallback_reason: None,
        context_window: 1_000_000,
        estimated_context_tokens: 120,
        context_usage_percent: 1,
        near_compaction_threshold: false,
        used_model_fallback: false,
        stream_retries: 0,
        turn_cost: deep_code_agent::CostEstimate {
            cny: 0.001,
            usd: 0.0001,
        },
        session_cost: deep_code_agent::CostEstimate {
            cny: 0.002,
            usd: 0.0002,
        },
    });

    let status = app.status_line();
    assert!(status.contains("ready"));
    assert!(status.contains("session session_1"));
    assert!(status.contains("checkpoint checkpoint_1"));
    assert!(status.contains("auto->deepseek-v4-flash"));
    assert!(status.contains("total ¥0.0020"));
    assert!(status.contains("ctx 1%"));
}

#[test]
fn status_line_shows_plan_marker_when_active() {
    let app = App::new();
    assert!(!app.status_line().contains("计划"));
    app.plan_mode.set(true);
    assert!(app.status_line().contains("计划/只读"));
}

#[test]
fn plan_mode_survives_runtime_relaunch() {
    let mut app = App::new();
    app.plan_mode.set(true);
    // A relaunch (/apikey, /model) adopts a freshly-built runtime; plan mode
    // must carry over onto the new handle instead of silently resetting.
    let workspace = tempfile::tempdir().unwrap();
    let launched = deep_code_agent::launch_runtime(
        &deep_code_agent::AgentConfig::default(),
        workspace.path().to_path_buf(),
        None,
    );
    app.adopt_runtime(launched);
    assert!(app.plan_mode.active(), "plan mode must survive relaunch");
}

#[test]
fn assistant_delta_renders_exactly_once() {
    let mut app = App::new();
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    app.apply_runtime_event(RuntimeEvent::TurnStarted {
        turn_id: turn_id.clone(),
        prompt: "hi".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::AssistantDelta {
        turn_id: turn_id.clone(),
        text: "hel".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::AssistantDelta {
        turn_id: turn_id.clone(),
        text: "lo".to_string(),
    });
    app.apply_runtime_event(RuntimeEvent::TurnFinished {
        turn_id,
        usage: None,
        telemetry: None,
    });
    let assistant_cells = app
        .history
        .iter()
        .filter(|cell| matches!(cell, HistoryCell::Assistant { .. }))
        .count();
    assert_eq!(assistant_cells, 1, "one delta stream → one assistant cell");
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::Assistant { text }) if text == "hello"
    ));
}
