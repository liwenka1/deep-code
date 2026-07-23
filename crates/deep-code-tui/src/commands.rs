use deep_code_agent::{
    CheckpointId, CheckpointStore, CostCurrency, JsonSessionStore, PrefixStatus, RuntimeEvent,
    SessionStore, TurnTelemetry, format_sessions_storage_note, web_enabled,
};

use crate::app::App;
use crate::history::HistoryCell;
use crate::i18n::{Lang, TextId, tr, tr_with};

/// (command, hint id, takes an argument). Single source for the completion
/// menu and `/help`; hints are looked up from the language pack when the menu
/// or `/help` text is built.
pub(crate) const SLASH_COMMANDS: &[(&str, TextId, bool)] = &[
    ("/help", TextId::HintHelp, false),
    ("/clear", TextId::HintClear, false),
    ("/status", TextId::HintStatus, false),
    ("/model", TextId::HintModel, true),
    ("/apikey", TextId::HintApikey, true),
    ("/logout", TextId::HintLogout, false),
    ("/copy", TextId::HintCopy, false),
    ("/checkpoints", TextId::HintCheckpoints, false),
    ("/restore", TextId::HintRestore, true),
    ("/resume", TextId::HintResume, false),
    ("/sessions", TextId::HintSessions, false),
    ("/agents", TextId::HintAgents, false),
    ("/find", TextId::HintFind, true),
    ("/lang", TextId::HintLang, true),
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
                    self.status = self.tr(TextId::UsageRestore).to_string();
                } else {
                    self.restore_checkpoint(id);
                }
                true
            }
            _ if prompt == "/find" || prompt.starts_with("/find ") => {
                let query = prompt.strip_prefix("/find").unwrap_or_default().trim();
                if query.is_empty() {
                    self.status = self.tr(TextId::UsageFind).to_string();
                } else {
                    self.find_in_transcript(query);
                }
                true
            }
            _ if prompt == "/lang" || prompt.starts_with("/lang ") => {
                let arg = prompt.strip_prefix("/lang").unwrap_or_default().trim();
                self.set_lang_command(arg);
                true
            }
            _ => false,
        }
    }

    /// `/lang`: show the current UI language; `/lang zh|en` switches it live
    /// and persists the choice to the global config.
    fn set_lang_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let message = self.tr_with(TextId::LangCurrent, &[("lang", self.tr(TextId::LangName))]);
            self.history.push(HistoryCell::system(message.clone()));
            self.status = message;
            return;
        }
        // Reuse the same tag parser as auto-detection so `/lang zh_CN`,
        // `/lang english`, etc. resolve consistently instead of a second
        // hardcoded "zh"/"en" table.
        let Some(lang) = Lang::from_tag(arg) else {
            self.status = self.tr_with(TextId::LangUnknown, &[("value", arg)]);
            return;
        };
        let update = deep_code_agent::GlobalConfigUpdate::Language(lang.as_setting().to_string());
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(_) => {
                // Switch immediately — the transcript re-renders from
                // structured cells, so history follows on the next frame.
                self.lang = lang;
                let message = tr(lang, TextId::LangSwitched).to_string();
                self.history.push(HistoryCell::system(message.clone()));
                self.status = message;
            }
            Err(message) => self.status = message,
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
        let update = deep_code_agent::GlobalConfigUpdate::ApiKey(Some(arg.trim().to_string()));
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(path) => match self.relaunch_runtime() {
                Ok(()) => {
                    self.history.push(HistoryCell::system(self.tr_with(
                        TextId::ApiKeySaved,
                        &[
                            ("path", &path.display().to_string()),
                            ("backend", &self.backend_label),
                        ],
                    )));
                    self.status =
                        self.tr_with(TextId::StatusConnected, &[("backend", &self.backend_label)]);
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
            self.history.push(HistoryCell::system(self.tr_with(
                TextId::ModelCurrent,
                &[
                    ("model", &self.configured_model),
                    ("available", &available()),
                ],
            )));
            self.status = self.tr(TextId::StatusModelInfoShown).to_string();
            return;
        }
        let resolved = match arg.to_ascii_lowercase().as_str() {
            "auto" => "auto".to_string(),
            "pro" => deep_code_agent::DEEPSEEK_V4_PRO.to_string(),
            "flash" => deep_code_agent::DEEPSEEK_V4_FLASH.to_string(),
            _ => match registry.info_for(arg) {
                Some(info) => info.id.clone(),
                None => {
                    self.status = self.tr_with(
                        TextId::ModelUnknown,
                        &[("name", arg), ("available", &available())],
                    );
                    return;
                }
            },
        };
        let update = deep_code_agent::GlobalConfigUpdate::Model(resolved.clone());
        match deep_code_agent::write_global_config_update(&self.global_config_path, &update) {
            Ok(_) => match self.relaunch_runtime() {
                Ok(()) => {
                    self.history.push(HistoryCell::system(self.tr_with(
                        TextId::ModelSwitched,
                        &[("model", &resolved), ("backend", &self.backend_label)],
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
                    self.history
                        .push(HistoryCell::system(self.tr(TextId::LogoutDone)));
                    self.status =
                        self.tr_with(TextId::StatusLoggedOut, &[("backend", &self.backend_label)]);
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
                self.status = self.tr_with(
                    TextId::CopiedResponse,
                    &[("count", &text.chars().count().to_string())],
                );
            }
            None => self.status = self.tr(TextId::NothingToCopy).to_string(),
        }
    }

    fn show_help(&mut self) {
        let mut text = self.tr(TextId::HelpHeader).to_string();
        for (command, hint, _) in SLASH_COMMANDS {
            text.push('\n');
            text.push_str(command);
            text.push_str(" - ");
            text.push_str(tr(self.lang, *hint));
        }
        for note in [
            TextId::HelpTipSessions,
            TextId::HelpKeys,
            TextId::HelpNoteCancel,
            TextId::HelpNoteAutoAllow,
        ] {
            text.push('\n');
            text.push_str(self.tr(note));
        }
        self.history.push(HistoryCell::system(text));
        self.status = self.tr(TextId::StatusHelpShown).to_string();
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
                let cache_line = self.tr_with(
                    TextId::StatusCacheHitLine,
                    &[
                        ("turn", &turn_cache),
                        ("session", &session_cache),
                        (
                            "saved",
                            &telemetry.session_cache_savings.format(self.cost_currency),
                        ),
                    ],
                );
                format!(
                    "\neffective_model={}\nroute={}\nroute_source={}\ncascade_triggered={}\nreasoning={}\nauto_reason={}\nturn_cost={}\nsession_cost={}\n{cache_line}\ncontext={}/{} ({}%)\ncompaction_near={}\nstream_retries={}{}",
                    telemetry.effective_model,
                    telemetry.route_label,
                    telemetry.route_source,
                    telemetry.cascade_triggered,
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
        let trimmed = if self.trimmed_cells > 0 {
            self.tr_with(
                TextId::StatusTrimmedSuffix,
                &[("count", &self.trimmed_cells.to_string())],
            )
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
        self.status = self.tr_with(
            TextId::StatusShown,
            &[("mode", mode), ("backend", &self.backend_label)],
        );
    }

    fn list_subagents(&mut self) {
        let manager = match self.subagent_manager.read() {
            Ok(manager) => manager,
            Err(error) => {
                self.status = self.tr_with(
                    TextId::SubagentsUnavailable,
                    &[("error", &error.to_string())],
                );
                return;
            }
        };
        let agents = manager.list_current_session();
        if agents.is_empty() {
            self.status = self.tr(TextId::NoSubagents).to_string();
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
        let running = manager.running_count();
        self.history.push(HistoryCell::system(format!(
            "{}\n{body}",
            self.tr(TextId::SubagentsHeader)
        )));
        self.status = self.tr_with(
            TextId::SubagentsCount,
            &[
                ("count", &agents.len().to_string()),
                ("running", &running.to_string()),
            ],
        );
    }

    pub(crate) fn refresh_subagent_status(&mut self) {
        if let Ok(manager) = self.subagent_manager.read() {
            let running = manager.running_count();
            if running > 0 {
                self.status = self.tr_with(
                    TextId::StatusReadySubagents,
                    &[
                        ("backend", &self.backend_label),
                        ("running", &running.to_string()),
                    ],
                );
            }
        }
    }

    fn list_sessions(&mut self) {
        match JsonSessionStore::for_workspace(self.workspace.clone()) {
            Ok(store) => match store.list() {
                Ok(records) if records.is_empty() => {
                    self.status = self.tr(TextId::NoSavedSessions).to_string();
                }
                Ok(records) => {
                    let note = format_sessions_storage_note(&self.workspace);
                    self.history.push(HistoryCell::system(format!(
                        "{note}\n{}\n{}",
                        self.tr(TextId::SessionsHeader),
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
                    self.status = self.tr_with(
                        TextId::SessionsCount,
                        &[("count", &records.len().to_string())],
                    );
                }
                Err(error) => {
                    self.status =
                        self.tr_with(TextId::ListFailed, &[("error", &error.to_string())]);
                }
            },
            Err(error) => {
                self.status = self.tr_with(
                    TextId::SessionsUnavailable,
                    &[("error", &error.to_string())],
                );
            }
        }
    }

    fn list_checkpoints(&mut self) {
        match CheckpointStore::new(&self.workspace) {
            Ok(store) => match store.list() {
                Ok(ids) if ids.is_empty() => {
                    self.status = self.tr(TextId::NoCheckpoints).to_string();
                }
                Ok(ids) => {
                    self.history.push(HistoryCell::system(format!(
                        "{}\n{}",
                        self.tr(TextId::CheckpointsHeader),
                        ids.iter()
                            .map(|id| format!("- {}", id.0))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )));
                    self.status = self.tr_with(
                        TextId::CheckpointsCount,
                        &[("count", &ids.len().to_string())],
                    );
                }
                Err(error) => {
                    self.status =
                        self.tr_with(TextId::ListFailed, &[("error", &error.to_string())]);
                }
            },
            Err(error) => {
                self.status = self.tr_with(
                    TextId::CheckpointsUnavailable,
                    &[("error", &error.to_string())],
                );
            }
        }
    }

    fn restore_checkpoint(&mut self, id: &str) {
        // Route through the runtime handle (the one place that owns the
        // configured checkpoint store) instead of keeping a second
        // CheckpointStore construction here.
        let checkpoint_id = CheckpointId(id.to_string());
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.status = self.tr(TextId::RestoreOutsideRuntime).to_string();
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
            Err(error) => {
                self.status = self.tr_with(TextId::RestoreFailed, &[("error", &error.to_string())]);
            }
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

/// The user-facing tag for the prompt-prefix cache status. Presentation
/// lives here, not in the agent crate — telemetry stays language-neutral.
fn prefix_status_label(status: PrefixStatus, lang: Lang) -> &'static str {
    match status {
        PrefixStatus::FirstTurn => tr(lang, TextId::PrefixFirstTurn),
        PrefixStatus::Stable => tr(lang, TextId::PrefixStable),
        PrefixStatus::Changed => tr(lang, TextId::PrefixChanged),
    }
}

pub(crate) fn format_turn_telemetry(
    telemetry: &TurnTelemetry,
    currency: CostCurrency,
    lang: Lang,
) -> String {
    let turn = telemetry.turn_cost.format(currency);
    let session = telemetry.session_cost.format(currency);
    let cache = match (telemetry.cache_hit_tokens, telemetry.cache_miss_tokens) {
        (Some(hit), Some(miss)) => match cache_hit_percent(hit, miss) {
            Some(pct) => tr_with(
                lang,
                TextId::TelemetryCacheHit,
                &[("pct", &pct.to_string())],
            ),
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
        tr(lang, TextId::TelemetryNearCompaction)
    } else {
        ""
    };
    let fallback = telemetry
        .fallback_reason
        .as_deref()
        .map(|reason| format!(" | {reason}"))
        .unwrap_or_default();
    let retries = if telemetry.stream_retries > 0 {
        tr_with(
            lang,
            TextId::TelemetryStreamRetries,
            &[("count", &telemetry.stream_retries.to_string())],
        )
    } else {
        String::new()
    };
    let cascade = if telemetry.cascade_triggered {
        tr(lang, TextId::TelemetryCascade)
    } else {
        ""
    };
    let costs = tr_with(
        lang,
        TextId::TelemetryTurnSession,
        &[("turn", &turn), ("session", &session)],
    );
    format!(
        " | {} | {} | {costs}{cache} | {context}{compaction} | {}{fallback}{retries}{cascade}",
        telemetry.route_label,
        telemetry.route_reason,
        prefix_status_label(telemetry.prefix_status, lang)
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
