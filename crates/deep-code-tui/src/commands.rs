use deep_code_agent::{
    CheckpointId, CheckpointStore, CostCurrency, JsonSessionStore, RuntimeEvent, SessionStore,
    TurnTelemetry, format_sessions_storage_note,
};

use crate::app::App;
use crate::history::HistoryCell;

/// (command, hint, takes an argument). Single source for the completion
/// menu and `/help`.
pub(crate) const SLASH_COMMANDS: &[(&str, &str, bool)] = &[
    ("/help", "显示帮助", false),
    ("/clear", "清空可见历史", false),
    ("/status", "显示运行状态", false),
    ("/checkpoints", "列出 checkpoints", false),
    ("/restore", "恢复 checkpoint <id>", true),
    ("/sessions", "列出会话", false),
    ("/agents", "列出子代理", false),
];

impl App {
    pub(crate) fn handle_slash_command(&mut self, prompt: &str) -> bool {
        match prompt {
            "/help" => {
                self.show_help();
                true
            }
            "/clear" => {
                self.history.clear();
                self.active_turn = None;
                self.status = "Cleared visible history.".to_string();
                true
            }
            "/status" => {
                self.show_status();
                true
            }
            "/checkpoints" => {
                self.list_checkpoints();
                true
            }
            "/sessions" => {
                self.list_sessions();
                true
            }
            "/agents" => {
                self.list_subagents();
                true
            }
            _ if prompt.starts_with("/restore ") => {
                let id = prompt.trim_start_matches("/restore ").trim();
                if id.is_empty() {
                    self.status = "Usage: /restore <checkpoint_id>".to_string();
                } else {
                    self.restore_checkpoint(id);
                }
                true
            }
            _ => false,
        }
    }

    fn show_help(&mut self) {
        self.history.push(HistoryCell::system(
            "Commands:\n/help - show this help\n/clear - clear visible history\n/status - show runtime status\n/checkpoints - list checkpoints\n/restore <id> - restore checkpoint\n/sessions - list sessions\n/agents - list sub-agents\nKeys: Enter send, Alt+Enter/Ctrl+J 换行, Ctrl+P/Ctrl+N 提示词历史, Esc 取消本轮/清空输入/退出 (审批面板中为 deny), Ctrl+C quit, PageUp/PageDown scroll, y/a/n approve/会话允许/deny.\n注意: 取消在工具边界生效，正在执行中的同步工具会先跑完；a 对 shell 类工具只做一次性批准。\n配置 [approval] auto_allow 可预先放行工具前缀（仅 env 或全局配置，项目配置无效）。",
        ));
        self.status = "Help displayed.".to_string();
    }

    fn show_status(&mut self) {
        let session = self.session_id.as_deref().unwrap_or("none");
        let checkpoint = self.last_checkpoint.as_deref().unwrap_or("none");
        let mode = if self.pending_approval.is_some() {
            "approval"
        } else if self.is_streaming {
            "streaming"
        } else {
            "ready"
        };
        let telemetry = self
            .last_telemetry
            .as_ref()
            .map(|telemetry| {
                let fallback = telemetry
                    .fallback_reason
                    .as_deref()
                    .map(|reason| format!("\nfallback_reason={reason}"))
                    .unwrap_or_default();
                format!(
                    "\neffective_model={}\nroute={}\nreasoning={}\nauto_reason={}\nturn_cost={}\nsession_cost={}\ncontext={}/{} ({}%)\ncompaction_near={}\nstream_retries={}{}",
                    telemetry.effective_model,
                    telemetry.route_label,
                    telemetry.reasoning_effort,
                    telemetry.route_reason,
                    telemetry.turn_cost.format(self.cost_currency),
                    telemetry.session_cost.format(self.cost_currency),
                    telemetry.estimated_context_tokens,
                    telemetry.context_window,
                    telemetry.context_usage_percent,
                    telemetry.near_compaction_threshold,
                    telemetry.stream_retries,
                    fallback
                )
            })
            .unwrap_or_else(|| "\nlast_turn=none".to_string());
        self.history.push(HistoryCell::system(format!(
            "Status:\nbackend={}\nsession={session}\ncheckpoint={checkpoint}\nmode={mode}\nconfigured_model={}\nconfigured_reasoning={}\nhistory_cells={}{}",
            self.backend_label,
            self.configured_model,
            self.configured_reasoning,
            self.history.len(),
            telemetry
        )));
        self.status = format!("Status: {mode} - {}", self.backend_label);
    }

    fn list_subagents(&mut self) {
        let manager = match self.subagent_manager.read() {
            Ok(manager) => manager,
            Err(error) => {
                self.status = format!("Sub-agents unavailable: {error}");
                return;
            }
        };
        let agents = manager.list_current_session();
        if agents.is_empty() {
            self.status = "No sub-agents in this session.".to_string();
            return;
        }
        let body = agents
            .iter()
            .map(|agent| {
                let handle = agent
                    .transcript_handle
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("-");
                format!(
                    "- {} [{}] {} | handle={handle} | {}",
                    agent.name,
                    agent.status.as_str(),
                    agent.role,
                    agent.short_summary()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.history
            .push(HistoryCell::system(format!("Sub-agents:\n{body}")));
        self.status = format!(
            "{} sub-agent(s), {} running",
            agents.len(),
            manager.running_count()
        );
    }

    pub(crate) fn refresh_subagent_status(&mut self) {
        if let Ok(manager) = self.subagent_manager.read() {
            let running = manager.running_count();
            if running > 0 {
                self.status = format!(
                    "Ready - {} | {} sub-agent(s) running",
                    self.backend_label, running
                );
            }
        }
    }

    fn list_sessions(&mut self) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for sessions.".to_string();
            return;
        };
        match JsonSessionStore::for_workspace(cwd) {
            Ok(store) => match store.list() {
                Ok(records) if records.is_empty() => {
                    self.status = "No saved sessions.".to_string();
                }
                Ok(records) => {
                    let note = std::env::current_dir()
                        .map(|cwd| format_sessions_storage_note(&cwd))
                        .unwrap_or_default();
                    self.history.push(HistoryCell::system(format!(
                        "{note}\nSessions:\n{}",
                        records
                            .iter()
                            .map(|record| {
                                format!(
                                    "- {} ({} msgs) {}",
                                    record.id.as_str(),
                                    record.messages.len(),
                                    record.preview().replace('\n', " ")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    )));
                    self.status =
                        format!("{} session(s). CLI: deep-code session list", records.len());
                }
                Err(error) => self.status = format!("List failed: {error}"),
            },
            Err(error) => self.status = format!("Sessions unavailable: {error}"),
        }
    }

    fn list_checkpoints(&mut self) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for checkpoints.".to_string();
            return;
        };
        match CheckpointStore::new(cwd) {
            Ok(store) => match store.list() {
                Ok(ids) if ids.is_empty() => {
                    self.status = "No checkpoints yet.".to_string();
                }
                Ok(ids) => {
                    self.history.push(HistoryCell::system(format!(
                        "Checkpoints:\n{}",
                        ids.iter()
                            .map(|id| format!("- {}", id.0))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )));
                    self.status = format!("{} checkpoint(s).", ids.len());
                }
                Err(error) => self.status = format!("List failed: {error}"),
            },
            Err(error) => self.status = format!("Checkpoints unavailable: {error}"),
        }
    }

    fn restore_checkpoint(&mut self, id: &str) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for restore.".to_string();
            return;
        };
        let checkpoint_id = CheckpointId(id.to_string());
        match CheckpointStore::new(cwd) {
            Ok(store) => match store.restore(&checkpoint_id) {
                Ok(()) => {
                    self.last_checkpoint = Some(id.to_string());
                    self.apply_runtime_event(RuntimeEvent::WorkspaceRestored { id: checkpoint_id });
                }
                Err(error) => self.status = format!("Restore failed: {error}"),
            },
            Err(error) => self.status = format!("Checkpoints unavailable: {error}"),
        }
    }
}

pub(crate) fn format_turn_telemetry(telemetry: &TurnTelemetry, currency: CostCurrency) -> String {
    let turn = telemetry.turn_cost.format(currency);
    let session = telemetry.session_cost.format(currency);
    let cache = match (telemetry.cache_hit_tokens, telemetry.cache_miss_tokens) {
        (Some(hit), Some(miss)) => format!(" | cache {hit}/{miss}"),
        _ => String::new(),
    };
    let context = format!(
        "ctx {}/{} ({}%)",
        telemetry.estimated_context_tokens,
        telemetry.context_window,
        telemetry.context_usage_percent
    );
    let compaction = if telemetry.near_compaction_threshold {
        " | 接近压缩阈值"
    } else {
        ""
    };
    let fallback = telemetry
        .fallback_reason
        .as_deref()
        .map(|reason| format!(" | {reason}"))
        .unwrap_or_default();
    let retries = if telemetry.stream_retries > 0 {
        format!(" | 流重试 {} 次", telemetry.stream_retries)
    } else {
        String::new()
    };
    format!(
        " | {} | {} | 本回合 {} | 累计 {}{cache} | {context}{compaction} | {}{fallback}{retries}",
        telemetry.route_label,
        telemetry.route_reason,
        turn,
        session,
        telemetry.prefix_status.label_zh()
    )
}
