use deep_code_agent::{
    CheckpointId, CheckpointStore, CostCurrency, JsonSessionStore, RuntimeEvent, SessionStore,
    TurnTelemetry, format_sessions_storage_note, web_enabled,
};

use crate::app::App;
use crate::history::HistoryCell;

/// (command, hint, takes an argument). Single source for the completion
/// menu and `/help`.
pub(crate) const SLASH_COMMANDS: &[(&str, &str, bool)] = &[
    ("/help", "显示帮助", false),
    (
        "/clear",
        "开启新对话：重置上下文并清屏（旧对话可 /resume 找回）",
        false,
    ),
    ("/status", "显示运行状态", false),
    ("/plan", "切换计划模式（只读，拦截所有写/执行工具）", false),
    ("/model", "查看/切换模型 (auto|pro|flash)", true),
    ("/apikey", "设置 DeepSeek API key 并接入", true),
    ("/logout", "清除 API key 回离线模式", false),
    ("/copy", "复制最近一条助手回复到剪贴板", false),
    ("/checkpoints", "列出 checkpoints", false),
    ("/restore", "恢复 checkpoint <id>", true),
    ("/resume", "选择历史会话 (或手动输入 <id>)", false),
    ("/sessions", "列出会话", false),
    ("/agents", "列出子代理", false),
    ("/find", "搜索转录 <关键词>（重复执行向更早处翻）", true),
];

impl App {
    pub(crate) fn handle_slash_command(&mut self, prompt: &str) -> bool {
        match prompt {
            "/help" => {
                self.show_help();
                true
            }
            "/clear" => {
                self.start_new_conversation();
                true
            }
            "/plan" => {
                self.toggle_plan_mode();
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
            _ if prompt == "/find" || prompt.starts_with("/find ") => {
                let query = prompt.strip_prefix("/find").unwrap_or_default().trim();
                if query.is_empty() {
                    self.status = "Usage: /find <关键词>".to_string();
                } else {
                    self.find_in_transcript(query);
                }
                true
            }
            _ => false,
        }
    }

    fn toggle_plan_mode(&mut self) {
        let now_on = self.plan_mode.toggle();
        let message = if now_on {
            "计划模式已开启（只读）：写文件 / shell / 网络等工具会被拦截，请先给出计划。再次 /plan 退出。"
        } else {
            "计划模式已关闭：工具恢复正常执行（仍受审批与策略约束）。"
        };
        self.history.push(HistoryCell::system(message.to_string()));
        self.status = if now_on {
            "计划模式（只读）".to_string()
        } else {
            "计划模式已关闭".to_string()
        };
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
        let update = deep_code_agent::GlobalConfigUpdate::ApiKey(Some(arg.trim().to_string()));
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
            let mut ids: Vec<String> = registry
                .list()
                .iter()
                .map(|model| model.id.clone())
                .collect();
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
        let mut text = String::from("Commands:");
        for (command, hint, _) in SLASH_COMMANDS {
            text.push('\n');
            text.push_str(command);
            text.push_str(" - ");
            text.push_str(hint);
        }
        text.push_str(
            "\nTip: 会话存储于 .deep-code/sessions/。\nKeys: Enter send, Alt+Enter/Ctrl+J 换行, ↑↓ 滚动聊天 (多行草稿内移光标), Ctrl+P/Ctrl+N 历史, Ctrl+W 删词, Ctrl+U/K 删行, Ctrl+A/E 行首尾, Esc 取消本轮/清空输入/退出 (审批面板中为 deny), Ctrl+C 取消/清空/连按两次退出, Shift+↑/↓ 或 PageUp/PageDown 滚动正文 (鼠标可原生划选复制), y/a/n 审批, 审批面板中 ↑↓ 选择 Enter 确认.\n注意: 取消在工具边界生效，正在执行中的同步工具会先跑完；a 对 shell 按命令程序名在本会话放行（如 cargo/git，复合命令仍逐次询问）。\n配置 [approval] auto_allow 可预先放行工具前缀（仅 env 或全局配置，项目配置无效）。",
        );
        self.history.push(HistoryCell::system(text));
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
                let turn_cache = cache_hit_percent(
                    telemetry.cache_hit_tokens.unwrap_or(0),
                    telemetry.cache_miss_tokens.unwrap_or(0),
                )
                .map_or_else(|| "—".to_string(), |pct| format!("{pct}%"));
                let session_cache = cache_hit_percent(
                    telemetry.session_cache_hit_tokens,
                    telemetry.session_cache_miss_tokens,
                )
                .map_or_else(|| "—".to_string(), |pct| format!("{pct}%"));
                format!(
                    "\neffective_model={}\nroute={}\nroute_source={}\ncascade_triggered={}\nreasoning={}\nauto_reason={}\nturn_cost={}\nsession_cost={}\ncache_hit=本轮 {} · 会话 {}（已省 {}）\ncontext={}/{} ({}%)\ncompaction_near={}\nstream_retries={}{}",
                    telemetry.effective_model,
                    telemetry.route_label,
                    telemetry.route_source,
                    telemetry.cascade_triggered,
                    telemetry.reasoning_effort,
                    telemetry.route_reason,
                    telemetry.turn_cost.format(self.cost_currency),
                    telemetry.session_cost.format(self.cost_currency),
                    turn_cache,
                    session_cache,
                    telemetry.session_cache_savings.format(self.cost_currency),
                    telemetry.estimated_context_tokens,
                    telemetry.context_window,
                    telemetry.context_usage_percent,
                    telemetry.near_compaction_threshold,
                    telemetry.stream_retries,
                    fallback
                )
            })
            .unwrap_or_else(|| "\nlast_turn=none".to_string());
        let trimmed = if self.trimmed_cells > 0 {
            format!("（已折叠最早 {} 条）", self.trimmed_cells)
        } else {
            String::new()
        };
        self.history.push(HistoryCell::system(format!(
            "Status:\nbackend={}\nweb={}\nsession={session}\ncheckpoint={checkpoint}\nmode={mode}\nconfigured_model={}\nconfigured_reasoning={}\nhistory_cells={}{trimmed}{}",
            self.backend_label,
            if web_enabled() { "on" } else { "off" },
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
                format!(
                    "- {} [{}] {} | {}",
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
        match JsonSessionStore::for_workspace(self.workspace.clone()) {
            Ok(store) => match store.list() {
                Ok(records) if records.is_empty() => {
                    self.status = "No saved sessions.".to_string();
                }
                Ok(records) => {
                    let note = format_sessions_storage_note(&self.workspace);
                    self.history.push(HistoryCell::system(format!(
                        "{note}\nSessions:\n{}",
                        records
                            .iter()
                            .map(|record| {
                                format!(
                                    "- {} ({} msgs) {}",
                                    record.id.as_str(),
                                    record.message_count(),
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
        match CheckpointStore::new(&self.workspace) {
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
        // Route through the runtime handle (the one place that owns the
        // configured checkpoint store) instead of keeping a second
        // CheckpointStore construction here.
        let checkpoint_id = CheckpointId(id.to_string());
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.status = "Cannot restore outside the async runtime.".to_string();
            return;
        };
        let runtime = std::sync::Arc::clone(&self.runtime);
        let result = tokio::task::block_in_place(|| {
            handle.block_on(runtime.restore_checkpoint(checkpoint_id.clone()))
        });
        match result {
            Ok(()) => {
                self.last_checkpoint = Some(id.to_string());
                self.apply_runtime_event(RuntimeEvent::WorkspaceRestored { id: checkpoint_id });
            }
            Err(error) => self.status = format!("Restore failed: {error}"),
        }
    }
}

/// Cache hit rate as a percentage, or `None` when no prompt tokens were billed.
pub(crate) fn cache_hit_percent(hit: u32, miss: u32) -> Option<u8> {
    let total = u64::from(hit) + u64::from(miss);
    (u64::from(hit) * 100)
        .checked_div(total)
        .map(|percent| percent as u8)
}

pub(crate) fn format_turn_telemetry(telemetry: &TurnTelemetry, currency: CostCurrency) -> String {
    let turn = telemetry.turn_cost.format(currency);
    let session = telemetry.session_cost.format(currency);
    let cache = match (telemetry.cache_hit_tokens, telemetry.cache_miss_tokens) {
        (Some(hit), Some(miss)) => match cache_hit_percent(hit, miss) {
            Some(pct) => format!(" | 缓存命中 {pct}%"),
            None => String::new(),
        },
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
    let cascade = if telemetry.cascade_triggered {
        " | ⚡ 级联升级：Flash 工具调用连续失败，本会话改用 Pro"
    } else {
        ""
    };
    format!(
        " | {} | {} | 本回合 {} | 累计 {}{cache} | {context}{compaction} | {}{fallback}{retries}{cascade}",
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
    fn cache_hit_percent_handles_zero_and_rounds_down() {
        assert_eq!(cache_hit_percent(0, 0), None);
        assert_eq!(cache_hit_percent(80, 20), Some(80));
        assert_eq!(cache_hit_percent(1, 2), Some(33));
        assert_eq!(cache_hit_percent(100, 0), Some(100));
    }

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
