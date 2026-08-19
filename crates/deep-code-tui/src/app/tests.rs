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
    use deep_code_agent::SessionEntry;
    let make = |prompt: &str| {
        let mut record = SessionRecord::new(std::path::PathBuf::from("/tmp/ws"), "system");
        record.entries = vec![
            std::sync::Arc::new(SessionEntry::system("system")),
            std::sync::Arc::new(SessionEntry::user(prompt)),
        ];
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

/// A root-grant approval offers two options (y/n): the hidden "approve for
/// session" key must be inert, and the focus cycle must wrap at two.
#[test]
fn root_grant_approval_drops_the_session_option() {
    let mut app = App::new();
    app.pending_approval = Some(deep_code_agent::ApprovalRequest {
        network: false,
        call_id: "call_1".to_string(),
        tool_name: deep_code_agent::REQUEST_WRITE_ROOT_TOOL.to_string(),
        description: "widen".to_string(),
        arguments: serde_json::json!({ "path": "/tmp/x", "justification": "y" }),
        risk_level: deep_code_agent::RiskLevel::High,
        requires_sandbox: false,
        read_only: false,
        matched_rule: None,
        justification: Some("y".to_string()),
        preview: None,
        safety_notes: Vec::new(),
    });
    assert!(app.pending_is_root_grant());

    // 'a' is not an offered option, so the key must do nothing at all.
    app.approve_pending_tool_for_session();
    assert!(
        app.pending_approval.is_some(),
        "the hidden session-approve key must be inert for a root grant"
    );

    // Two options: focus wraps 0 → 1 → 0 in both directions.
    assert_eq!(app.approval_focus, 0);
    app.approval_focus_down();
    assert_eq!(app.approval_focus, 1);
    app.approval_focus_down();
    assert_eq!(app.approval_focus, 0);
    app.approval_focus_up();
    assert_eq!(app.approval_focus, 1);
}

#[test]
fn approval_scroll_helpers_adjust_panel_offset() {
    let mut app = App::new();
    app.pending_approval = Some(deep_code_agent::ApprovalRequest {
        network: false,
        call_id: "call_1".to_string(),
        tool_name: "write_file".to_string(),
        description: "Write a file".to_string(),
        arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
        risk_level: deep_code_agent::RiskLevel::High,
        requires_sandbox: true,
        read_only: false,
        matched_rule: Some("write".to_string()),
        justification: None,
        preview: None,
        safety_notes: Vec::new(),
    });
    for _ in 0..10 {
        app.scroll_approval_down();
    }
    // The stored offset is unclamped (the render layer clamps against the
    // real wrapped panel height); the helpers just move it.
    assert_eq!(app.approval_scroll_offset, 30);
    app.scroll_approval_up();
    assert_eq!(app.approval_scroll_offset, 27);
    app.scroll_approval_to_top();
    assert_eq!(app.approval_scroll_offset, 0);
    app.scroll_approval_up();
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
fn composer_edits_and_stays_live_while_streaming() {
    let mut app = App::new();
    app.push_char('a');
    app.push_newline();
    app.push_char('b');
    assert_eq!(app.input, "a\nb");

    // Inverted deliberately: this test used to assert "no edits while
    // streaming", which is the guard that made mid-turn steering unreachable —
    // `submit`'s queue branch needs a non-empty composer to fire. The composer
    // must stay editable mid-turn for steering to exist at all.
    app.is_streaming = true;
    app.push_char('c');
    app.push_newline();
    app.push_char('d');
    assert_eq!(
        app.input, "a\nbc\nd",
        "composer stays editable while streaming"
    );
    app.backspace();
    assert_eq!(app.input, "a\nbc\n");
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
fn shift_tab_cycles_permission_mode_with_yolo_confirm() {
    use deep_code_agent::PermissionMode;
    let mut app = App::new();
    assert_eq!(app.permission_mode(), PermissionMode::Default);

    app.cycle_permission_mode();
    assert_eq!(app.permission_mode(), PermissionMode::AcceptEdits);
    app.cycle_permission_mode();
    assert_eq!(app.permission_mode(), PermissionMode::Auto);

    // Cycling from Auto arms Yolo but does NOT enter it yet.
    app.cycle_permission_mode();
    assert_eq!(
        app.permission_mode(),
        PermissionMode::Auto,
        "armed, not entered"
    );
    assert!(app.yolo_armed);
    // A second consecutive Shift+Tab confirms Yolo.
    app.cycle_permission_mode();
    assert_eq!(app.permission_mode(), PermissionMode::Yolo);
    assert!(!app.yolo_armed);

    // From Yolo it wraps back to Default (no confirm to leave).
    app.cycle_permission_mode();
    assert_eq!(app.permission_mode(), PermissionMode::Default);
}

#[test]
fn any_other_key_disarms_pending_yolo() {
    use deep_code_agent::PermissionMode;
    let mut app = App::new();
    app.permission_mode.set(PermissionMode::Auto);
    app.cycle_permission_mode(); // arms yolo
    assert!(app.yolo_armed);
    app.clear_yolo_arm();
    assert!(!app.yolo_armed);
    // Next cycle from Auto arms again rather than entering yolo directly.
    app.cycle_permission_mode();
    assert!(app.yolo_armed);
    assert_eq!(app.permission_mode(), PermissionMode::Auto);
}

#[test]
fn lang_command_switches_live_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.global_config_path = dir.path().join("config.toml");
    assert_eq!(app.lang, Lang::Zh);

    // Bare /lang reports the current language without touching config.
    assert!(app.handle_slash_command("/lang"));
    assert!(app.status.contains("中文"), "{}", app.status);
    assert!(!app.global_config_path.exists());

    // /lang en switches immediately, persists, and confirms in English.
    assert!(app.handle_slash_command("/lang en"));
    assert_eq!(app.lang, Lang::En);
    let contents = std::fs::read_to_string(&app.global_config_path).unwrap();
    assert!(contents.contains("language = \"en\""));
    assert!(app.status.contains("English"), "{}", app.status);

    // Runtime swaps must not flip the language back.
    assert!(app.handle_slash_command("/clear"));
    assert_eq!(app.lang, Lang::En);
    assert!(app.status.contains("New conversation"), "{}", app.status);

    // Unknown value is rejected without changing anything.
    assert!(app.handle_slash_command("/lang jp"));
    assert_eq!(app.lang, Lang::En);
    assert!(app.status.contains("jp"), "{}", app.status);

    // And back to Chinese.
    assert!(app.handle_slash_command("/lang zh"));
    assert_eq!(app.lang, Lang::Zh);
    assert!(
        std::fs::read_to_string(&app.global_config_path)
            .unwrap()
            .contains("language = \"zh\"")
    );
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
        network: false,
        call_id: "call_1".to_string(),
        tool_name: "mock_echo".to_string(),
        description: "echo".to_string(),
        arguments: serde_json::json!({}),
        risk_level: deep_code_agent::RiskLevel::Low,
        requires_sandbox: false,
        read_only: true,
        matched_rule: None,
        justification: None,
        preview: None,
        safety_notes: Vec::new(),
    });

    app.approve_pending_tool_for_session();

    assert!(app.pending_approval.is_none());
    assert!(app.is_streaming);
    assert!(app.status.contains("已批准（本会话）"), "{}", app.status);
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

/// While a tool executes, the activity label must name the tool and tick its
/// own clock — "生成中 180s" over a minutes-long `agent` call reads as a hang,
/// which is exactly the report that motivated this.
#[test]
fn streaming_activity_names_the_running_tool() {
    use crate::active_turn::{ActiveToolCell, ActiveTurn, LiveOutput};
    use deep_code_agent::{ToolCallId, TurnId};

    let mut app = App::new();
    app.is_streaming = true;
    app.streaming_since = Some(std::time::Instant::now());

    let mut turn = ActiveTurn::new(TurnId("turn_1".to_string()));
    turn.upsert_tool(ActiveToolCell {
        tool_call_id: ToolCallId("call_1".to_string()),
        tool_name: "agent".to_string(),
        arguments: "{}".to_string(),
        risk_level: None,
        requires_sandbox: None,
        approval: crate::history::ToolApprovalState::NotRequired,
        live_output: LiveOutput::default(),
        started_at: std::time::Instant::now(),
    });
    app.active_turn = Some(turn);

    let activity = app.streaming_activity().expect("indicator while running");
    assert!(
        activity.contains("agent") && activity.contains("运行中"),
        "tool wait must name the tool: {activity:?}"
    );

    // Tool finished and removed → falls back to the generating label.
    app.active_turn.as_mut().unwrap().tools.clear();
    let activity = app.streaming_activity().expect("indicator while streaming");
    assert!(activity.contains("生成中"), "{activity:?}");
}

#[test]
fn steering_queues_prompt_while_streaming_without_sending() {
    let mut app = App::new();
    app.is_streaming = true;
    let history_before = app.history.len();

    app.input = "wait, skip the tests".to_string();
    app.submit();

    assert_eq!(app.steering_queue, vec!["wait, skip the tests"]);
    assert!(app.input.is_empty(), "composer clears after queueing");
    assert!(app.is_streaming, "queueing must not end the current turn");
    assert_eq!(
        app.history.len(),
        history_before,
        "queued prompt is not shown until the turn finishes (ordering)"
    );
    assert!(app.status.contains("排队"));
}

// `flush_steering_queue` starts a real turn (`tokio::spawn`), so this needs a
// runtime; only the synchronous pre-spawn state is asserted.
#[tokio::test]
async fn steering_flush_sends_combined_queue_after_turn() {
    let mut app = App::new();
    app.is_streaming = true;
    app.input = "first".to_string();
    app.submit();
    app.input = "second".to_string();
    app.submit();
    assert_eq!(app.steering_queue.len(), 2);

    // Turn ends: the queue drains into one combined follow-up, now shown.
    app.is_streaming = false;
    app.flush_steering_queue();

    assert!(app.steering_queue.is_empty(), "queue drains on flush");
    assert!(app.is_streaming, "flush starts the follow-up turn");
    assert!(
        matches!(app.history.last(), Some(HistoryCell::User { text }) if text == "first\n\nsecond"),
        "combined queued prompt is appended after the finished turn"
    );
}

#[test]
fn steering_queue_cleared_on_error() {
    let mut app = App::new();
    app.is_streaming = true;
    app.input = "queued".to_string();
    app.submit();
    assert_eq!(app.steering_queue.len(), 1);

    app.record_error("boom".to_string());
    assert!(
        app.steering_queue.is_empty(),
        "a failed turn drops queued prompts rather than firing them into an error"
    );
    assert!(!app.is_streaming);
}

/// The steering flush starts a new turn, so it must survive the drain loop that
/// triggered it. Previously the trailing `StreamFinished` of the turn that just
/// ended nulled the successor's receiver, leaving that turn running with no UI
/// attached: tools executed and cost accrued invisibly, and an approval request
/// would have parked forever. Drives the real `drain_stream_updates` path.
#[tokio::test]
async fn steering_flush_survives_the_drain_that_triggered_it() {
    let mut app = App::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.ui_rx = Some(rx);
    app.is_streaming = true;

    app.input = "and then deploy".to_string();
    app.submit();
    assert_eq!(app.steering_queue.len(), 1);

    // Exactly what the bridge task emits at the end of a turn: the terminal
    // event, then the stream-closed marker, back to back in one drain pass.
    let turn_id = deep_code_agent::TurnId("turn_1".to_string());
    tx.send(UiUpdate::Event(Box::new(RuntimeEvent::TurnFinished {
        turn_id,
        usage: None,
        telemetry: None,
    })))
    .unwrap();
    tx.send(UiUpdate::StreamFinished).unwrap();

    app.drain_stream_updates();

    assert!(
        app.steering_queue.is_empty(),
        "queue flushed after the turn"
    );
    assert!(app.is_streaming, "the follow-up turn is live");
    assert!(
        app.ui_rx.is_some(),
        "the follow-up turn must keep its receiver — otherwise it runs orphaned"
    );
    assert!(
        matches!(app.history.last(), Some(HistoryCell::User { text }) if text == "and then deploy"),
        "the queued prompt is shown after the finished turn's cells"
    );
}

#[test]
fn steering_queue_dropped_when_stream_closes_without_a_terminal_event() {
    let mut app = App::new();
    app.is_streaming = true;
    app.input = "queued".to_string();
    app.submit();
    assert_eq!(app.steering_queue.len(), 1);

    // Channel closed with no TurnFinished/TurnCancelled/Error: there is no
    // finished turn to attach a follow-up to, so it must not linger and fire at
    // some later, unrelated turn (which would also reorder it).
    app.apply_ui_update(UiUpdate::StreamFinished);

    assert!(app.steering_queue.is_empty());
    assert!(!app.pending_steering_flush);
    assert!(!app.is_streaming);
}

#[test]
fn cancel_clears_the_steering_queue_synchronously() {
    let mut app = App::new();
    app.is_streaming = true;
    app.input = "never mind".to_string();
    app.submit();
    assert_eq!(app.steering_queue.len(), 1);

    // Esc/Ctrl+C means "changed my mind". Waiting for `TurnCancelled` to clear
    // the queue loses the race when the turn already finished: `cancel_turn` is
    // a no-op on an idle runtime, so no `TurnCancelled` ever arrives.
    app.handle_escape();

    assert!(app.steering_queue.is_empty());
    assert!(!app.pending_steering_flush);
}

#[test]
fn steering_queue_is_capped_and_keeps_the_draft() {
    let mut app = App::new();
    app.is_streaming = true;
    for i in 0..STEERING_QUEUE_CAP {
        app.input = format!("msg {i}");
        app.submit();
    }
    assert_eq!(app.steering_queue.len(), STEERING_QUEUE_CAP);

    app.input = "one too many".to_string();
    app.submit();
    assert_eq!(
        app.steering_queue.len(),
        STEERING_QUEUE_CAP,
        "queue stops growing at the cap"
    );
    assert_eq!(
        app.input, "one too many",
        "the refused draft stays in the composer rather than vanishing"
    );
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
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "write_file".to_string(),
            description: "Write note.txt".to_string(),
            arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
            risk_level: deep_code_agent::RiskLevel::High,
            requires_sandbox: true,
            read_only: false,
            matched_rule: Some("write".to_string()),
            justification: None,
            preview: None,
            safety_notes: Vec::new(),
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
    // The approval itself is surfaced only by the dedicated panel
    // (app.pending_approval), never as a transcript cell.
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
fn status_line_is_minimal_model_and_context() {
    let mut app = App::new();
    // Session id, checkpoint, and cost all exist but must NOT appear on the
    // always-on bar — it's CC-minimal (model + context). They live in `/status`.
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
    // The effective model in use, plus context headroom — nothing else.
    assert!(status.contains("deepseek-v4-flash"), "{status}");
    assert!(status.contains("ctx 1%"), "{status}");
    // Cost, session id, checkpoint, and the verbose route label are gone.
    assert!(!status.contains("session"), "{status}");
    assert!(!status.contains("checkpoint"), "{status}");
    assert!(!status.contains('¥'), "{status}");
    assert!(!status.contains("auto->"), "{status}");
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

#[test]
fn add_dir_command_refuses_bad_input_without_relaunching() {
    let mut app = App::new();
    let original_session = app.session_id.clone();

    // Empty argument → usage note.
    assert!(app.handle_slash_command("/add-dir"));
    assert!(app.status.contains("用法"), "status: {}", app.status);

    // Unresolvable path → resolve error, nothing granted.
    assert!(app.handle_slash_command("/add-dir does-not-exist-anywhere"));
    assert!(app.status.contains("无法解析"), "status: {}", app.status);

    // The workspace itself grants nothing → friendly no-op.
    let ws = app.workspace.clone();
    assert!(app.handle_slash_command(&format!("/add-dir {}", ws.display())));
    assert!(app.status.contains("工作区本身"), "status: {}", app.status);

    assert!(app.extra_roots.is_empty());
    assert_eq!(
        app.session_id, original_session,
        "refused input must not relaunch the runtime"
    );
}

#[test]
fn add_dir_blocked_while_streaming() {
    let mut app = App::new();
    app.is_streaming = true;
    let dir = tempfile::tempdir().unwrap();
    assert!(app.handle_slash_command(&format!("/add-dir {}", dir.path().display())));
    assert!(app.extra_roots.is_empty());
    assert!(app.status.contains("流式"), "status: {}", app.status);
}

#[test]
fn add_dir_grants_relaunches_and_persists_immediately() {
    let mut app = App::new();
    let session_before = app.session_id.clone().expect("test sessions persist");
    let extra = tempfile::tempdir().unwrap();
    let canonical = extra.path().canonicalize().unwrap();

    assert!(app.handle_slash_command(&format!("/add-dir {}", extra.path().display())));

    assert_eq!(app.extra_roots, vec![canonical.clone()]);
    assert!(app.status.contains("已授权"), "status: {}", app.status);
    // The transcript names the new boundary right where the action happened.
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains(&canonical.display().to_string())
    ));
    // Same session resumed, and the grant is already on disk — not parked
    // until a turn's persist().
    assert_eq!(app.session_id.as_deref(), Some(session_before.as_str()));
    let store = JsonSessionStore::for_workspace(app.workspace.clone()).unwrap();
    let record = store
        .load(&deep_code_agent::SessionId::parse(&session_before).unwrap())
        .unwrap();
    assert_eq!(record.extra_roots, vec![canonical.clone()]);

    // Granting the same directory again is a no-op with a note.
    assert!(app.handle_slash_command(&format!("/add-dir {}", extra.path().display())));
    assert!(
        app.status.contains("已在授权列表"),
        "status: {}",
        app.status
    );
    assert_eq!(app.extra_roots.len(), 1);
}

#[test]
fn clear_carries_grants_into_the_new_session() {
    let mut app = App::new();
    let extra = tempfile::tempdir().unwrap();
    let canonical = extra.path().canonicalize().unwrap();
    assert!(app.handle_slash_command(&format!("/add-dir {}", extra.path().display())));
    let granted_session = app.session_id.clone();

    assert!(app.handle_slash_command("/clear"));

    assert_ne!(
        app.session_id, granted_session,
        "clear starts a new session"
    );
    // Grants are process-scoped: the human granted them for this run, so the
    // fresh session inherits them (and is born with them persisted).
    assert_eq!(app.extra_roots, vec![canonical.clone()]);
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains(&canonical.display().to_string())
    ));
    let store = JsonSessionStore::for_workspace(app.workspace.clone()).unwrap();
    let id = app.session_id.clone().expect("new session persists");
    let record = store
        .load(&deep_code_agent::SessionId::parse(&id).unwrap())
        .unwrap();
    assert_eq!(record.extra_roots, vec![canonical]);
}

#[test]
fn switch_session_adopts_and_banners_the_records_grants() {
    let mut app = App::new();
    let extra = tempfile::tempdir().unwrap();
    let canonical = extra.path().canonicalize().unwrap();
    // A session in this workspace carrying its own grant, created out of band
    // (as if by an earlier `--add-dir` run).
    let store = JsonSessionStore::for_workspace(app.workspace.clone()).unwrap();
    let record = SessionRecord::new(app.workspace.clone(), "system")
        .with_extra_roots(vec![canonical.clone()]);
    store.save(&record).unwrap();

    app.switch_session(record).unwrap();

    // The display state must describe the adopted runtime's boundary, not the
    // (grantless) one this App launched with — and say so in the transcript.
    assert_eq!(app.extra_roots, vec![canonical.clone()]);
    assert!(matches!(
        app.history.last(),
        Some(HistoryCell::System { text }) if text.contains(&canonical.display().to_string())
    ));
}
