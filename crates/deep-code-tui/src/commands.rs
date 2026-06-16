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
    ("/model", "查看/切换模型 (auto|pro|flash)", true),
    ("/apikey", "设置 DeepSeek API key 并接入", true),
    ("/logout", "清除 API key 回离线模式", false),
    ("/copy", "复制最近一条助手回复到剪贴板", false),
    ("/checkpoints", "列出 checkpoints", false),
    ("/restore", "恢复 checkpoint <id>", true),
    ("/resume", "选择历史会话 (或手动输入 <id>)", false),
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
                self.clear_selection();
                self.status = "Cleared visible history.".to_string();
                true
            }
            "/status" => {
                self.show_status();
                true
            }
            "/copy" => {
                self.copy_last_response();
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
            "/logout" => {
                self.logout();
                true
            }
            _ if prompt == "/model" || prompt.starts_with("/model ") => {
                let arg = prompt.strip_prefix("/model").unwrap_or_default().trim();
                self.set_model(arg);
                true
            }
            _ if prompt == "/apikey" || prompt.starts_with("/apikey ") => {
                let arg = prompt.strip_prefix("/apikey").unwrap_or_default().trim();
                self.set_api_key(arg);
                true
            }
            _ if prompt == "/resume" || prompt.starts_with("/resume ") => {
                let arg = prompt.strip_prefix("/resume").unwrap_or_default().trim();
                self.resume_session_command(arg);
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

    fn resume_session_command(&mut self, arg: &str) {
        if arg.is_empty() {
            self.open_resume_picker();
        } else if let Err(message) = self.switch_session_by_id(arg) {
            self.status = message;
        }
    }

    fn set_api_key(&mut self, arg: &str) {
        if let Err(message) = deep_code_agent::validate_api_key(arg) {
            self.status = message;
            return;
        }
        let update =
            deep_code_agent::GlobalConfigUpdate::ApiKey(Some(arg.trim().to_string()));
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(path) => match self.relaunch_runtime() {
                Ok(()) => {
                    self.history.push(HistoryCell::system(format!(
                        "API key 已保存至 {}（权限 600）。当前后端: {}",
                        path.display(),
                        self.backend_label
                    )));
                    self.status = format!("已接入 - {}", self.backend_label);
                }
                Err(message) => self.status = message,
            },
            Err(message) => self.status = message,
        }
    }

    fn set_model(&mut self, arg: &str) {
        let registry = deep_code_agent::ModelRegistry::default();
        let available = || {
            let mut ids: Vec<String> =
                registry.list().iter().map(|model| model.id.clone()).collect();
            ids.push("auto".to_string());
            ids.join(", ")
        };
        if arg.is_empty() {
            self.history.push(HistoryCell::system(format!(
                "当前模型: {}\n用法: /model <auto|pro|flash|完整 id>\n可用: {}",
                self.configured_model,
                available()
            )));
            self.status = "Model info displayed.".to_string();
            return;
        }
        let resolved = match arg.to_ascii_lowercase().as_str() {
            "auto" => "auto".to_string(),
            "pro" => deep_code_agent::DEEPSEEK_V4_PRO.to_string(),
            "flash" => deep_code_agent::DEEPSEEK_V4_FLASH.to_string(),
            _ => match registry.info_for(arg) {
                Some(info) => info.id.clone(),
                None => {
                    self.status = format!("未知模型 '{arg}'。可用: {}", available());
                    return;
                }
            },
        };
        let update = deep_code_agent::GlobalConfigUpdate::Model(resolved.clone());
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(_) => match self.relaunch_runtime() {
                Ok(()) => {
                    self.history.push(HistoryCell::system(format!(
                        "模型已切换为 {resolved}（已写入全局配置）。当前后端: {}",
                        self.backend_label
                    )));
                    self.status = format!("model = {resolved} - {}", self.backend_label);
                }
                Err(message) => self.status = message,
            },
            Err(message) => self.status = message,
        }
    }

    fn logout(&mut self) {
        let update = deep_code_agent::GlobalConfigUpdate::ApiKey(None);
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(_) => match self.relaunch_runtime() {
                Ok(()) => {
                    self.history.push(HistoryCell::system(
                        "已清除 API key，回到离线模式。/apikey sk-xxx 可重新接入。".to_string(),
                    ));
                    self.status = format!("已登出 - {}", self.backend_label);
                }
                Err(message) => self.status = message,
            },
            Err(message) => self.status = message,
        }
    }

    fn copy_last_response(&mut self) {
        let last = self.history.iter().rev().find_map(|cell| match cell {
            HistoryCell::Assistant { text } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        match last {
            Some(text) => {
                crate::clipboard::copy(&text);
                self.status = format!("已复制最近回复到剪贴板 ({} 字)", text.chars().count());
            }
            None => self.status = "没有可复制的助手回复".to_string(),
        }
    }

    fn show_help(&mut self) {
        self.history.push(HistoryCell::system(
            "Commands:\n/help - show this help\n/clear - clear visible history\n/status - show runtime status\n/model <auto|pro|flash> - 切换模型并写入全局配置\n/apikey <sk-...> - 设置 API key 并就地接入\n/logout - 清除 API key 回离线\n/copy - 复制最近一条助手回复\n/resume [id] - 切换历史会话 (留空弹出选择器)\n/checkpoints - list checkpoints\n/restore <id> - restore checkpoint\n/sessions - list sessions\n/agents - list sub-agents\nKeys: Enter send, Alt+Enter/Ctrl+J 换行, ↑↓ 滚动聊天 (多行草稿内移光标), Ctrl+P/Ctrl+N 历史, Ctrl+W 删词, Ctrl+U/K 删行, Ctrl+A/E 行首尾, Esc 取消本轮/清空输入/退出 (审批面板中为 deny), Ctrl+C 取消/清空/连按两次退出, Shift+↑/↓ 或 PageUp/PageDown 滚动正文 (鼠标可原生划选复制), y/a/n approve/会话允许/deny.\n注意: 取消在工具边界生效，正在执行中的同步工具会先跑完；a 对 shell 类工具只做一次性批准。\n配置 [approval] auto_allow 可预先放行工具前缀（仅 env 或全局配置，项目配置无效）。",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_last_response_picks_latest_assistant() {
        let mut app = App::new();
        app.history.push(HistoryCell::assistant("first answer"));
        app.history.push(HistoryCell::user("a question"));
        app.history.push(HistoryCell::assistant("second answer"));
        app.copy_last_response();
        assert!(app.status.contains("已复制"));

        let mut empty = App::new();
        empty.copy_last_response();
        assert!(empty.status.contains("没有可复制"));
    }
}
