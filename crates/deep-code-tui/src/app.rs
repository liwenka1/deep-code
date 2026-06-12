//! TUI application state.
//!
//! This module is intentionally thin: the agent runtime owns the model loop,
//! tool registry, session, and approval gating. The UI only has to:
//!
//! 1. forward user prompts via [`AgentRuntimeHandle::submit_user`],
//! 2. render [`RuntimeEvent`]s as they arrive,
//! 3. forward approval decisions via [`AgentRuntimeHandle::submit_approval`].

use std::sync::Arc;

use deep_code_agent::{
    AgentConfig, AgentRuntimeHandle, ApprovalDecision, ApprovalRequest, CostCurrency, RuntimeEvent,
    SessionRecord, SharedSubAgentManager, TurnTelemetry, format_sessions_storage_note,
    launch_runtime,
};
use tokio::sync::mpsc;

use crate::active_turn::ActiveTurn;
use crate::cli::workspace_root;
use crate::history::{HistoryCell, hydrate_history};

#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub resume: Option<SessionRecord>,
}

/// Updates pushed from the bridge task into the UI thread.
#[derive(Debug, Clone, PartialEq)]
enum UiUpdate {
    Event(Box<RuntimeEvent>),
    StreamFinished,
}

enum StreamRequest {
    User(String),
    Approval(ApprovalDecision),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    Slash,
    File,
}

/// Inline completion menu state: `/` commands or `@` workspace files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionMenu {
    pub(crate) kind: CompletionKind,
    /// (completion value, hint)
    pub(crate) items: Vec<(String, String)>,
    pub(crate) selected: usize,
}

const COMPLETION_MENU_ITEMS: usize = 8;

type UiUpdateReceiver = mpsc::UnboundedReceiver<UiUpdate>;

pub struct App {
    pub input: String,
    pub history: Vec<HistoryCell>,
    pub active_turn: Option<ActiveTurn>,
    pub status: String,
    pub error: Option<String>,
    pub should_quit: bool,
    pub is_streaming: bool,
    pub pending_approval: Option<ApprovalRequest>,
    pub last_checkpoint: Option<String>,
    pub session_id: Option<String>,
    pub(crate) resumed: bool,
    pub scroll_offset: usize,
    pub approval_scroll_offset: usize,
    pub(crate) runtime: Arc<dyn AgentRuntimeHandle>,
    pub(crate) backend_label: String,
    pub(crate) subagent_manager: SharedSubAgentManager,
    subagent_shutdown: Option<Box<dyn Fn() + Send + Sync>>,
    ui_rx: Option<UiUpdateReceiver>,
    pub(crate) cost_currency: CostCurrency,
    pub(crate) configured_model: String,
    pub(crate) configured_reasoning: String,
    pub(crate) last_telemetry: Option<TurnTelemetry>,
    pub(crate) prompt_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    pub(crate) completion: Option<CompletionMenu>,
    pub(crate) workspace_files: Vec<String>,
}

const PROMPT_HISTORY_CAP: usize = 100;

impl App {
    #[must_use]
    pub fn launch(config: LaunchConfig) -> Self {
        let workspace = workspace_root();
        let workspace_files = deep_code_agent::list_workspace_files(&workspace, 2000);
        let loaded = AgentConfig::load(&workspace);
        let config_warnings = loaded.report.warnings.clone();
        let agent_config = loaded.config;
        let cost_currency = agent_config.cost_currency;
        let configured_model = agent_config.model.clone();
        let configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        let launched = launch_runtime(&agent_config, workspace, config.resume.clone());
        let runtime = launched.handle;
        let backend_label = launched.backend_label;
        let session_id = launched.session_id;
        let subagent_manager = launched.subagent_manager;
        let subagent_shutdown = Some(launched.stop_hook);
        let resumed = config.resume.is_some();
        let persistent = session_id.is_some();
        let workspace_note = std::env::current_dir()
            .map(|cwd| format_sessions_storage_note(&cwd))
            .unwrap_or_else(|_| {
                "Sessions are stored under .deep-code/sessions/ in the current workspace directory."
                    .to_string()
            });
        let mut history = vec![HistoryCell::system(format!(
            "{}\n{}\n{}\n{}\n{}",
            "Type a prompt and press Enter. Esc: 取消本轮/清空输入/退出 (按状态依次生效), Ctrl+C: 退出.",
            "Tip: in offline mode, type \"/mock-tool hello\" to exercise approval.",
            "Slash: /checkpoints, /restore <id>, /sessions, /agents",
            workspace_note,
            if resumed {
                "Resumed previous session."
            } else if persistent {
                "Started a new persistent session."
            } else {
                "Session persistence unavailable in this workspace."
            }
        ))];

        if !config_warnings.is_empty() {
            history.push(HistoryCell::system(format!(
                "配置警告:\n{}",
                config_warnings.join("\n")
            )));
        }

        if let Some(record) = config.resume.as_ref() {
            history.extend(hydrate_history(record));
        }

        let status = if let Some(id) = &session_id {
            if resumed {
                format!("Ready (resumed) - {backend_label} | session {id}")
            } else {
                format!("Ready - {backend_label} | session {id}")
            }
        } else {
            format!("Ready - {backend_label}")
        };

        Self {
            input: String::new(),
            history,
            active_turn: None,
            status,
            error: None,
            should_quit: false,
            is_streaming: false,
            pending_approval: None,
            last_checkpoint: None,
            session_id,
            resumed,
            scroll_offset: 0,
            approval_scroll_offset: 0,
            runtime,
            backend_label,
            subagent_manager,
            subagent_shutdown,
            ui_rx: None,
            cost_currency,
            configured_model,
            configured_reasoning,
            last_telemetry: None,
            prompt_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            completion: None,
            workspace_files,
        }
    }

    #[must_use]
    pub fn new() -> Self {
        Self::launch(LaunchConfig::default())
    }

    pub fn push_char(&mut self, value: char) {
        if !self.is_streaming {
            self.input.push(value);
            self.history_cursor = None;
            self.refresh_completion();
        }
    }

    pub fn backspace(&mut self) {
        if !self.is_streaming {
            self.input.pop();
            self.history_cursor = None;
            self.refresh_completion();
        }
    }

    /// Insert a newline into the composer (Alt+Enter / Ctrl+J).
    pub fn push_newline(&mut self) {
        if !self.is_streaming {
            self.input.push('\n');
            self.history_cursor = None;
            self.refresh_completion();
        }
    }

    #[must_use]
    pub(crate) fn completion_open(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn close_completion(&mut self) {
        self.completion = None;
    }

    /// Recompute the menu from the current input: `/command` prefix while no
    /// whitespace was typed, or a trailing `@file` token.
    fn refresh_completion(&mut self) {
        self.completion = self.compute_completion();
    }

    fn compute_completion(&self) -> Option<CompletionMenu> {
        if let Some(rest) = self.input.strip_prefix('/') {
            if self.input.contains(char::is_whitespace) {
                return None;
            }
            let filter = rest.to_lowercase();
            let items: Vec<(String, String)> = crate::commands::SLASH_COMMANDS
                .iter()
                .filter(|(name, _, _)| name[1..].starts_with(&filter))
                .map(|(name, hint, takes_arg)| {
                    let value = if *takes_arg {
                        format!("{name} ")
                    } else {
                        (*name).to_string()
                    };
                    (value, (*hint).to_string())
                })
                .collect();
            return (!items.is_empty()).then_some(CompletionMenu {
                kind: CompletionKind::Slash,
                items,
                selected: 0,
            });
        }

        let token_start = self.trailing_token_start();
        let token = &self.input[token_start..];
        let filter = token.strip_prefix('@')?;
        let filter_lower = filter.to_lowercase();
        let mut matched: Vec<&String> = self
            .workspace_files
            .iter()
            .filter(|file| file.to_lowercase().contains(&filter_lower))
            .collect();
        matched.sort_by_key(|file| {
            (
                !file.to_lowercase().starts_with(&filter_lower),
                file.len(),
                (*file).clone(),
            )
        });
        let items: Vec<(String, String)> = matched
            .into_iter()
            .take(COMPLETION_MENU_ITEMS)
            .map(|file| (file.clone(), String::new()))
            .collect();
        (!items.is_empty()).then_some(CompletionMenu {
            kind: CompletionKind::File,
            items,
            selected: 0,
        })
    }

    /// Byte index where the trailing whitespace-delimited token begins.
    fn trailing_token_start(&self) -> usize {
        self.input
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    }

    pub(crate) fn completion_up(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            let len = menu.items.len();
            menu.selected = (menu.selected + len - 1) % len;
        }
    }

    pub(crate) fn completion_down(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            menu.selected = (menu.selected + 1) % menu.items.len();
        }
    }

    /// Apply the selected completion to the input. Returns true when the
    /// completed value is a ready-to-run slash command (no argument), so the
    /// caller can submit immediately on Enter.
    pub(crate) fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some((value, _)) = menu.items.get(menu.selected) else {
            return false;
        };
        match menu.kind {
            CompletionKind::Slash => {
                self.input = value.clone();
                !value.ends_with(' ')
            }
            CompletionKind::File => {
                let token_start = self.trailing_token_start();
                self.input.truncate(token_start);
                self.input.push_str(value);
                self.input.push(' ');
                false
            }
        }
    }

    /// Composer area height including borders; content grows 1..=6 rows.
    #[must_use]
    pub fn input_height(&self) -> u16 {
        let rows = self.input.split('\n').count().clamp(1, 6) as u16;
        rows + 2
    }

    /// Recall the previous sent prompt (Ctrl+P). The live input is stashed as
    /// a draft and restored when navigating past the newest entry.
    pub fn history_prev(&mut self) {
        if self.is_streaming || self.prompt_history.is_empty() {
            return;
        }
        let cursor = match self.history_cursor {
            None => {
                self.history_draft = std::mem::take(&mut self.input);
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_cursor = Some(cursor);
        self.input = self.prompt_history[cursor].clone();
    }

    /// Walk back toward the draft (Ctrl+N).
    pub fn history_next(&mut self) {
        if self.is_streaming {
            return;
        }
        match self.history_cursor {
            None => {}
            Some(index) if index + 1 < self.prompt_history.len() => {
                self.history_cursor = Some(index + 1);
                self.input = self.prompt_history[index + 1].clone();
            }
            Some(_) => {
                self.history_cursor = None;
                self.input = std::mem::take(&mut self.history_draft);
            }
        }
    }

    fn remember_prompt(&mut self, prompt: &str) {
        if self.prompt_history.last().map(String::as_str) != Some(prompt) {
            self.prompt_history.push(prompt.to_string());
            if self.prompt_history.len() > PROMPT_HISTORY_CAP {
                self.prompt_history.remove(0);
            }
        }
        self.history_cursor = None;
        self.history_draft.clear();
    }

    pub fn submit(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            return;
        }
        self.close_completion();

        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            self.status = "Enter a prompt before sending.".to_string();
            return;
        }
        self.remember_prompt(&prompt);

        if prompt.starts_with('/') && self.handle_slash_command(&prompt) {
            self.input.clear();
            return;
        }

        self.input.clear();
        self.error = None;
        self.active_turn = None;
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = format!("Streaming from {}...", self.backend_label);

        self.history.push(HistoryCell::user(prompt.clone()));

        self.start_stream(StreamRequest::User(prompt));
    }

    /// Esc cancel stack: close completion menu > deny approval (handled by
    /// the approval branch in `ui::handle_key`) > cancel the streaming turn
    /// > clear input > quit.
    pub fn handle_escape(&mut self) {
        if self.completion_open() {
            self.close_completion();
        } else if self.is_streaming {
            self.cancel_streaming_turn();
        } else if !self.input.is_empty() {
            self.input.clear();
            self.history_cursor = None;
            self.status = "已清空输入 (再按 Esc 退出)".to_string();
        } else {
            self.should_quit = true;
        }
    }

    fn cancel_streaming_turn(&mut self) {
        self.status = "正在取消本轮... (取消在工具边界生效)".to_string();
        let runtime = Arc::clone(&self.runtime);
        // The streaming loop emits TurnCancelled on the live channel that the
        // bridge task is already pumping; the receiver returned here stays
        // empty, so it can be dropped.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime.cancel_turn().await;
            });
        }
    }

    pub fn approve_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Approved);
    }

    /// "a": approve and remember the tool for this session. The runtime
    /// downgrades shell-class tools to a one-time approve.
    pub fn approve_pending_tool_for_session(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::ApprovedForSession);
    }

    pub fn deny_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Denied);
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(3);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_approval_up(&mut self) {
        self.approval_scroll_offset = self.approval_scroll_offset.saturating_sub(3);
    }

    pub fn scroll_approval_down(&mut self) {
        self.approval_scroll_offset = self
            .approval_scroll_offset
            .saturating_add(3)
            .min(self.approval_scroll_max());
    }

    pub fn scroll_approval_to_top(&mut self) {
        self.approval_scroll_offset = 0;
    }

    #[must_use]
    pub fn clamped_approval_scroll_offset(&self) -> usize {
        self.approval_scroll_offset.min(self.approval_scroll_max())
    }

    pub(crate) fn approval_cell(&self) -> Option<HistoryCell> {
        self.pending_approval
            .as_ref()
            .map(|request| HistoryCell::Approval {
                tool_name: request.tool_name.clone(),
                description: request.description.clone(),
                risk_level: format!("{:?}", request.risk_level),
                requires_sandbox: request.requires_sandbox,
                matched_rule: request.matched_rule.clone(),
                arguments: request.arguments.to_string(),
            })
    }

    fn approval_scroll_max(&self) -> usize {
        self.approval_cell()
            .map(|cell| cell.lines().len().saturating_sub(1))
            .unwrap_or(0)
    }

    /// Apply queued runtime updates; returns whether anything changed (the
    /// render loop uses this to skip redundant redraws).
    pub fn drain_stream_updates(&mut self) -> bool {
        let Some(mut rx) = self.ui_rx.take() else {
            return false;
        };

        let mut applied = false;
        while let Ok(update) = rx.try_recv() {
            self.apply_ui_update(update);
            applied = true;
        }

        if self.is_streaming {
            self.ui_rx = Some(rx);
        }
        applied
    }

    fn resolve_pending_tool(&mut self, decision: ApprovalDecision) {
        if self.pending_approval.take().is_none() {
            return;
        }

        let label = match decision {
            ApprovalDecision::Approved => "approved",
            ApprovalDecision::ApprovedForSession => "approved (session)",
            ApprovalDecision::Denied => "denied",
        };
        self.approval_scroll_offset = 0;
        self.status = format!("Tool {label}, resuming...");
        self.is_streaming = true;
        self.start_stream(StreamRequest::Approval(decision));
    }

    fn start_stream(&mut self, request: StreamRequest) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);

        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let mut events = match request {
                StreamRequest::User(prompt) => runtime.submit_user(prompt).await,
                StreamRequest::Approval(decision) => runtime.submit_approval(decision).await,
            };

            while let Some(event) = events.recv().await {
                if tx.send(UiUpdate::Event(Box::new(event.clone()))).is_err() {
                    return;
                }
                if matches!(
                    event,
                    RuntimeEvent::TurnFinished { .. }
                        | RuntimeEvent::TurnCancelled { .. }
                        | RuntimeEvent::ApprovalRequired { .. }
                        | RuntimeEvent::Error { .. }
                ) {
                    break;
                }
            }

            let _ = tx.send(UiUpdate::StreamFinished);
        });
    }

    fn apply_ui_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Event(event) => self.apply_runtime_event(*event),
            UiUpdate::StreamFinished => {
                self.is_streaming = false;
                self.ui_rx = None;
            }
        }
    }

    pub(crate) fn record_error(&mut self, message: String) {
        // Keep any streamed partial content visible: flush the active turn
        // into history before appending the error cell, otherwise the next
        // TurnStarted would silently discard it.
        self.flush_active_turn();
        self.error = Some(message.clone());
        self.status = "Agent error.".to_string();
        self.history
            .push(HistoryCell::system(format!("Error: {message}")));
        self.is_streaming = false;
        self.clear_stream_receiver();
    }

    pub(crate) fn clear_stream_receiver(&mut self) {
        self.ui_rx = None;
    }

    #[must_use]
    pub fn status_line(&self) -> String {
        let mode = if self.error.is_some() {
            "error"
        } else if self.pending_approval.is_some() {
            "approval"
        } else if self.is_streaming {
            "streaming"
        } else if self.resumed {
            "ready (resumed)"
        } else {
            "ready"
        };
        let session = self
            .session_id
            .as_deref()
            .map(|id| format!(" | session {id}"))
            .unwrap_or_else(|| " | session none".to_string());
        let checkpoint = self
            .last_checkpoint
            .as_deref()
            .map(|id| format!(" | checkpoint {id}"))
            .unwrap_or_default();
        let telemetry = self
            .last_telemetry
            .as_ref()
            .map(|value| {
                format!(
                    " | {} | turn {} | total {}",
                    value.route_label,
                    value.turn_cost.format(self.cost_currency),
                    value.session_cost.format(self.cost_currency)
                )
            })
            .unwrap_or_default();
        format!(
            "{mode} - {}{session}{checkpoint} | {}{telemetry}",
            self.backend_label, self.status
        )
    }

    pub async fn shutdown_runtime(&self) {
        if let Some(shutdown) = &self.subagent_shutdown {
            shutdown();
        }
        self.runtime.shutdown().await;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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

        assert!(app.handle_slash_command("/clear"));
        assert!(app.history.is_empty());
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
            prefix_status: deep_code_agent::PrefixStatus::Stable,
            route_reason: "短提示优先使用 Flash".to_string(),
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
            result: result.clone(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolResult { result });

        let tool_call_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
            .expect("tool call cell");
        let tool_result_indices = app
            .history
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                matches!(cell, HistoryCell::ToolResult { .. }).then_some(index)
            })
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
        assert!(app.active_turn.as_ref().is_some_and(|active| active.tools.is_empty()));
    }

    #[test]
    fn composer_newline_and_height_clamp() {
        let mut app = App::new();
        assert_eq!(app.input_height(), 3, "empty input is one row plus borders");

        app.push_char('a');
        app.push_newline();
        app.push_char('b');
        assert_eq!(app.input, "a\nb");
        assert_eq!(app.input_height(), 4);

        for _ in 0..10 {
            app.push_newline();
        }
        assert_eq!(app.input_height(), 8, "content rows clamp at 6 (+2 borders)");

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
        });

        app.approve_pending_tool_for_session();

        assert!(app.pending_approval.is_none());
        assert!(app.is_streaming);
        assert!(app.status.contains("approved (session)"));
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
            .position(
                |cell| matches!(cell, HistoryCell::System { text } if text.contains("boom")),
            )
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
        assert!(preview.iter().any(|cell| matches!(
            cell,
            HistoryCell::Approval {
                matched_rule: Some(rule),
                ..
            } if rule == "write"
        )));
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
            prefix_status: deep_code_agent::PrefixStatus::Stable,
            route_reason: "short prompt".to_string(),
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
    }

    #[test]
    fn provider_text_is_fallback_without_duplicating_structured_delta() {
        let mut app = App::new();
        app.apply_runtime_event(RuntimeEvent::Provider(
            deep_code_agent::AgentEvent::TextDelta {
                text: "legacy".to_string(),
            },
        ));
        app.apply_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: deep_code_agent::TurnId("turn_legacy".to_string()),
            usage: None,
            telemetry: None,
        });
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::Assistant { text }) if text == "legacy"
        ));

        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "hi".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            text: "hello".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::Provider(
            deep_code_agent::AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
        ));
        app.apply_runtime_event(RuntimeEvent::TurnFinished {
            turn_id,
            usage: None,
            telemetry: None,
        });
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::Assistant { text }) if text == "hello"
        ));
    }
}
