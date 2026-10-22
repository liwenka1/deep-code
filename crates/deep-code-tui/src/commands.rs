use deep_code_agent::{
    CheckpointId, CheckpointStore, CostEstimate, JsonSessionStore, Message, PrefixStatus,
    RuntimeEvent, SessionStore, estimate_token_count, format_sessions_storage_note, web_enabled,
};

use std::sync::Arc;

use crate::app::App;
use crate::history::HistoryCell;
use deep_code_agent::i18n::{Lang, TextId, tr};

/// The `/init` brief. Model-facing, so bilingual like the runtime's other
/// prompt text and never localized; the file it asks for is read by
/// `deep_code_agent::ProjectInstructions` at the next launch.
const INIT_BRIEF: &str = "\
请为这个仓库生成（或改进）项目指令文件 AGENTS.md。它会在每个会话开始时注入系统提示，\
所以只写对后续工作真正有用、又不能从代码一眼看出的事实。\n\
1. 先调查：目录结构、构建/测试/lint 命令、代码风格与命名约定、提交规范、测试布局、容易踩的坑。\n\
2. 如果工作区根目录已有 AGENTS.md，先读取并在它的基础上改进，不要丢弃已有内容。\n\
3. 用 write_file 写入工作区根目录的 AGENTS.md：200 行以内，按小节组织（项目概览 / 常用命令 / \
约定 / 注意事项），具体、可执行。\n\
4. 最后用两三句话说明写了什么，以及哪些内容你不确定、需要人确认。\n\
/ Generate (or improve) AGENTS.md for this repository. It is injected into the system prompt at \
the start of every session, so keep only facts that help later work and cannot be read off the \
code at a glance. Investigate first (layout, build/test/lint commands, style and naming \
conventions, commit rules, test layout, pitfalls); if an AGENTS.md already exists, read it and \
improve it without dropping existing content; write <workspace>/AGENTS.md with write_file — under \
200 lines, organised in sections (overview / commands / conventions / pitfalls), concrete and \
actionable; finish with two or three sentences on what you wrote and what you are unsure about.";

/// What `/skill <name> <arguments>` substitutes in a skill body, when present.
pub(crate) const SKILL_ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

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
    ("/compact", TextId::HintCompact, true),
    ("/context", TextId::HintContext, false),
    ("/init", TextId::HintInit, false),
    ("/skill", TextId::HintSkill, true),
    ("/diff", TextId::HintDiff, true),
    ("/lang", TextId::HintLang, true),
    ("/add-dir", TextId::HintAddDir, true),
    ("/image", TextId::HintImage, true),
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
            // `starts_with("/apikey")`, not `"/apikey "`: pasting the key right
            // after the command with no space (`/apikeysk-…`) — or an IME
            // full-width space — otherwise fell through to the model and was
            // persisted in the session transcript. `trim()` handles the
            // full-width space too (it strips all Unicode whitespace). No other
            // command shares this prefix, so nothing legitimate is captured.
            _ if prompt.starts_with("/apikey") => {
                let arg = prompt.strip_prefix("/apikey").unwrap_or_default().trim();
                self.set_api_key(arg);
                true
            }
            _ if prompt == "/resume" || prompt.starts_with("/resume ") => {
                let arg = prompt.strip_prefix("/resume").unwrap_or_default().trim();
                self.resume_session_command(arg);
                true
            }
            _ if prompt == "/add-dir" || prompt.starts_with("/add-dir ") => {
                let arg = prompt.strip_prefix("/add-dir").unwrap_or_default().trim();
                self.add_dir_command(arg);
                true
            }
            _ if prompt == "/restore" || prompt.starts_with("/restore ") => {
                let id = prompt.strip_prefix("/restore").unwrap_or_default().trim();
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
            _ if prompt == "/compact" || prompt.starts_with("/compact ") => {
                let focus = prompt.strip_prefix("/compact").unwrap_or_default().trim();
                self.compact_now_command((!focus.is_empty()).then_some(focus));
                true
            }
            "/context" => {
                self.context_command();
                true
            }
            "/init" => {
                self.init_command();
                true
            }
            _ if prompt == "/skill" || prompt.starts_with("/skill ") => {
                let arg = prompt.strip_prefix("/skill").unwrap_or_default().trim();
                self.skill_command(arg);
                true
            }
            _ if prompt == "/diff" || prompt.starts_with("/diff ") => {
                let arg = prompt.strip_prefix("/diff").unwrap_or_default().trim();
                self.diff_command(arg);
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

    /// `/init`: ask the model to draft (or improve) this repository's AGENTS.md.
    ///
    /// A canned brief sent as an ordinary turn — the model investigates and
    /// writes the file through its normal tools, so the write goes through the
    /// same approval gate as any other edit. The transcript shows `/init`; the
    /// brief itself lands in the session record like any prompt. Refused while
    /// a turn is running: the brief must not be queued behind unrelated work,
    /// and steering it into a live turn would mix two tasks.
    fn init_command(&mut self) {
        if self.runtime_busy() {
            self.status = self.tr(TextId::StatusInitBusy).to_string();
            return;
        }
        self.start_user_turn(
            INIT_BRIEF.to_string(),
            Vec::new(),
            Some("/init".to_string()),
        );
    }

    /// `/skill`: list the discovered skills, or run one by name.
    ///
    /// Running a skill sends its instructions as a turn of the user's own:
    /// `$ARGUMENTS` in the body is replaced by whatever followed the name, or
    /// the arguments are appended when the body has no placeholder. The body
    /// is the launch-time copy — the same text the prompt's index was built
    /// from — so an edit shows up with the next session, like the index. The
    /// transcript shows the command line; the session record keeps the brief.
    /// Refused while a turn is running, like `/init`, for the same reason.
    fn skill_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let skills = Arc::clone(&self.skills);
            let text = if skills.is_empty() {
                self.tr(TextId::SkillListEmpty).to_string()
            } else {
                let mut text = self.tr(TextId::SkillListHeader).to_string();
                for skill in skills.list() {
                    text.push_str("\n  ");
                    text.push_str(&skill.name);
                    if !skill.description.is_empty() {
                        text.push_str(" — ");
                        text.push_str(&skill.description);
                    }
                }
                text
            };
            self.history.push(HistoryCell::system(text));
            return;
        }
        let (name, arguments) = arg
            .split_once(char::is_whitespace)
            .map_or((arg, ""), |(name, rest)| (name, rest.trim()));
        let skills = Arc::clone(&self.skills);
        let Some(skill) = skills.get(name) else {
            self.status = self.tr_with(TextId::StatusSkillUnknown, &[("name", name)]);
            return;
        };
        if self.runtime_busy() {
            self.status = self.tr(TextId::StatusSkillBusy).to_string();
            return;
        }
        let body = if skill.body.contains(SKILL_ARGUMENTS_PLACEHOLDER) {
            skill.body.replace(SKILL_ARGUMENTS_PLACEHOLDER, arguments)
        } else if arguments.is_empty() {
            skill.body.clone()
        } else {
            format!("{}\n\n参数 / Arguments: {arguments}", skill.body)
        };
        let text = format!(
            "按 skill「{name}」执行 / Follow the skill \"{name}\":\n\n{body}",
            name = skill.name
        );
        let shown = format!("/skill {arg}");
        self.start_user_turn(text, Vec::new(), Some(shown));
    }

    /// `/diff [path]`: the working tree against `HEAD`, through the user's own
    /// git, rendered like a write's diff and opened in full — the user asked
    /// for it. Untracked files are named separately: they are not in a diff,
    /// and forgetting them is the usual way a change ships incomplete.
    fn diff_command(&mut self, arg: &str) {
        use crate::git_diff::{GitDiffError, working_tree_diff};
        let scope = (!arg.is_empty()).then_some(arg);
        let report = match working_tree_diff(&self.workspace, scope) {
            Ok(report) => report,
            Err(GitDiffError::GitMissing) => {
                let text = self.tr(TextId::DiffGitMissing).to_string();
                self.history.push(HistoryCell::system(text));
                return;
            }
            Err(GitDiffError::NotARepository) => {
                let text = self.tr(TextId::DiffNotGitRepo).to_string();
                self.history.push(HistoryCell::system(text));
                return;
            }
            Err(GitDiffError::Failed(error)) => {
                let text = self.tr_with(TextId::DiffFailed, &[("error", &error)]);
                self.history.push(HistoryCell::system(text));
                return;
            }
        };
        if report.diff.trim().is_empty() && report.untracked.is_empty() {
            let text = self.tr(TextId::DiffClean).to_string();
            self.history.push(HistoryCell::system(text));
            return;
        }
        if !report.diff.trim().is_empty() {
            let path = scope.map_or_else(
                || self.tr(TextId::DiffWorkingTreeLabel).to_string(),
                str::to_string,
            );
            self.history.push(HistoryCell::Diff {
                path,
                diff: report.diff,
                added: report.added,
                removed: report.removed,
                truncated: report.truncated,
                new_file: false,
                expanded: true,
            });
        }
        if !report.untracked.is_empty() {
            const LISTED: usize = 20;
            let mut files = report.untracked[..report.untracked.len().min(LISTED)].join(", ");
            if report.untracked.len() > LISTED {
                files.push_str(", …");
            }
            let text = self.tr_with(TextId::DiffUntracked, &[("files", &files)]);
            self.history.push(HistoryCell::system(text));
        }
    }

    /// `/compact`: fold old history into a summary now, whether or not the
    /// automatic threshold has been reached.
    ///
    /// The threshold is a token count, but the user often knows something it
    /// does not — that the last twenty turns were a dead-end exploration, or
    /// that a big task is about to start and the window should be cleared
    /// first. Compaction is not otherwise reachable by hand.
    fn compact_now_command(&mut self, focus: Option<&str>) {
        // Same busy guard as the other state-changing commands: a compaction
        // swaps the session's entry list under the running turn, and the
        // summary cell would then land above output that already streamed.
        // (The runtime keeps the in-flight request alive either way —
        // the fold never archives the newest user entry — so this is about
        // keeping what is on screen true, not about safety.)
        if self.runtime_busy() {
            self.status = self.tr(TextId::BusyCompact).to_string();
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.status = self.tr(TextId::CompactOutsideRuntime).to_string();
            return;
        }
        self.start_compaction(focus);
    }

    /// What `/compact` shows once the runtime answers: the fold's cell with
    /// the token counts either side, after a warning when the summary is the
    /// mechanical kind; or the honest "nothing to do" when the history was
    /// already short — the fold deliberately refuses to archive its own
    /// summary, so that is a normal answer, and saying it is the difference
    /// between a command that worked and one that looks broken.
    pub(crate) fn finish_compaction(&mut self, report: Option<deep_code_agent::CompactionReport>) {
        match report {
            Some(report) => {
                if let Some(reason) = &report.fallback_reason {
                    let text = self.tr_with(TextId::CompactionFallback, &[("reason", reason)]);
                    self.history.push(HistoryCell::system(text));
                }
                self.history.push(HistoryCell::compaction(
                    report.archived_entries,
                    Some((report.tokens_before, report.tokens_after)),
                    report.summary,
                ));
                self.status = self.tr_with(
                    TextId::StatusCompacted,
                    &[("count", &report.archived_entries.to_string())],
                );
            }
            None => {
                self.status = self.tr(TextId::CompactedNothing).to_string();
            }
        }
    }

    /// `/context`: dump what the model's context actually holds right now.
    ///
    /// The transcript on screen is NOT it. Compaction trims the session while
    /// leaving every cell above intact, so from the first compaction onward the
    /// two diverge — and there was no way to see the side the model actually
    /// reads. That is the gap `/compact` opens and this closes.
    fn context_command(&mut self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.status = self.tr(TextId::ContextUnavailable).to_string();
            return;
        };
        let runtime = std::sync::Arc::clone(&self.runtime);
        // The wire messages ARE the context (the provider sees nothing else), so
        // this lists them rather than the entries or the transcript.
        let (messages, tokens) = tokio::task::block_in_place(|| {
            handle.block_on(async {
                let messages = runtime.session_messages().await;
                let tokens = estimate_token_count(&messages);
                (messages, tokens)
            })
        });
        let header = self.tr_with(
            TextId::ContextHeader,
            &[
                ("messages", &messages.len().to_string()),
                ("tokens", &tokens.to_string()),
            ],
        );
        let mut body = vec![header.clone()];
        body.extend(self.context_list_lines(&messages));
        self.history.push(HistoryCell::system(body.join("\n")));
        self.status = header;
    }

    /// The `/context` listing: the first few wire messages, an elision marker,
    /// the last few.
    ///
    /// Head *and* tail, because that is the shape a compacted context has — the
    /// folded summary sits at the front and the retained tail at the back, and
    /// seeing exactly that is the point of the command.
    pub(crate) fn context_list_lines(&self, messages: &[Message]) -> Vec<String> {
        const HEAD: usize = 3;
        const TAIL: usize = 10;
        const LINE_CHARS: usize = 120;

        let line = |index: usize, message: &Message| {
            // Newlines would break the one-line-per-message shape this listing
            // exists to provide.
            let head =
                crate::history::truncate_chars(&message.content.replace('\n', " "), LINE_CHARS);
            // The wire role (`system`/`user`/`assistant`/`tool`), not a
            // translation: this is a dump of the wire, and the reason to read it
            // is to compare it against the transcript above.
            let calls = if message.tool_calls.is_empty() {
                String::new()
            } else {
                format!(" (+{} tool call(s))", message.tool_calls.len())
            };
            // Images are part of the wire too, and they are the one thing a
            // reader of this listing cannot see in the text: the content shows a
            // `[图片 #1 PNG]` chip where the picture sits, and the tokens those
            // pictures cost (up to 1024 each) would otherwise appear from
            // nowhere. Counted, not shown — a data URL is megabytes of base64
            // and would defeat the one-line-per-message shape.
            //
            // Counted before hydration, so this is what the *transcript* holds
            // rather than what the request will carry: an image the request had
            // to leave out (over the budget, or a model that cannot take one) is
            // still counted here and shows up as a sentence in `content` that
            // this listing has already truncated past. The listing answers "what
            // is in the sessions's history", and `/status` plus the per-turn
            // telemetry are where the request's own size is read.
            let images = if message.images.is_empty() {
                String::new()
            } else {
                format!(" (+{} image(s))", message.images.len())
            };
            let role = message.role.as_str();
            if head.is_empty() {
                format!("{index:>3}. {role}{calls}{images}")
            } else {
                format!("{index:>3}. {role}{calls}{images}: {head}")
            }
        };

        if messages.len() <= HEAD + TAIL {
            return messages
                .iter()
                .enumerate()
                .map(|(index, message)| line(index + 1, message))
                .collect();
        }
        let mut lines: Vec<String> = messages
            .iter()
            .take(HEAD)
            .enumerate()
            .map(|(index, message)| line(index + 1, message))
            .collect();
        lines.push(self.tr_with(
            TextId::ContextElided,
            &[("count", &(messages.len() - HEAD - TAIL).to_string())],
        ));
        let start = messages.len() - TAIL;
        lines.extend(
            messages
                .iter()
                .skip(start)
                .enumerate()
                .map(|(offset, message)| line(start + offset + 1, message)),
        );
        lines
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
        match deep_code_agent::write_global_config_update(
            &self.global_config_path,
            &update,
            self.lang,
        ) {
            Ok(_) => {
                // Switch the TUI immediately and push the new language into the
                // running runtime (its cached UI language for error diagnostics
                // and approval previews). A lock-free atomic via the handle —
                // no relaunch, so the live session is never at risk.
                self.lang = lang;
                self.runtime.set_ui_lang(lang);
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
        if let Err(message) = deep_code_agent::validate_api_key(arg, self.lang) {
            self.status = message;
            return;
        }
        let update = deep_code_agent::GlobalConfigUpdate::ApiKey(Some(arg.trim().to_string()));
        match deep_code_agent::write_global_config_update(
            &self.global_config_path,
            &update,
            self.lang,
        ) {
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
            ids.push(deep_code_agent::AUTO_MODEL.to_string());
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
        // `auto` is the one name here that is NOT a model: it is a sentinel the
        // turn router answers, so the catalog has no entry for it. Every real
        // name — canonical id or alias, including the `pro` / `flash` short
        // names the README advertises — is looked up in the catalog, so `/model`
        // and `provider.model` cannot disagree about which spellings exist.
        //
        // This used to carry its own `pro` / `flash` arms, which is precisely
        // how the two came to disagree: the arms shadowed the alias table, so
        // removing an alias there left `/model` still accepting a name that
        // config, env and the bot had stopped resolving.
        let resolved = if arg.eq_ignore_ascii_case(deep_code_agent::AUTO_MODEL) {
            deep_code_agent::AUTO_MODEL.to_string()
        } else {
            match registry.info_for(arg) {
                Some(info) => info.id.clone(),
                None => {
                    self.status = self.tr_with(
                        TextId::ModelUnknown,
                        &[("name", arg), ("available", &available())],
                    );
                    return;
                }
            }
        };
        let update = deep_code_agent::GlobalConfigUpdate::Model(resolved.clone());
        match deep_code_agent::write_global_config_update(
            &self.global_config_path,
            &update,
            self.lang,
        ) {
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
        match deep_code_agent::write_global_config_update(
            &self.global_config_path,
            &update,
            self.lang,
        ) {
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
                // Sanitized like everything else the model wrote. This text
                // goes to the system clipboard and from there, typically,
                // into a terminal or an editor — an unfiltered `\x1b` or `\r`
                // is a paste that repaints or submits itself. Tabs and
                // newlines survive; see `sanitize_for_clipboard`.
                let text = crate::ui::render::sanitize_for_clipboard(&text);
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
        let telemetry = match self.last_telemetry.as_ref() {
            Some(telemetry) => {
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
                let cache_line = self.cache_hit_line(
                    &turn_cache,
                    &session_cache,
                    telemetry.session_cache_hit_tokens,
                    telemetry.session_cache_miss_tokens,
                    telemetry.session_cache_savings,
                );
                format!(
                    "\neffective_model={}\nroute={}\nroute_source={}\nprefix={}\ncascade_triggered={}\nreasoning={}\nauto_reason={}\nturn_cost={}\nsession_cost={}\n{cache_line}\ncontext={}/{} ({}%)\ncompaction_near={}\nstream_retries={}{}",
                    telemetry.effective_model,
                    telemetry.route_label,
                    telemetry.route_source,
                    prefix_status_label(telemetry.prefix_status, self.lang),
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
            }
            // No turn has run in THIS session yet: a fresh launch, or just after
            // `/resume`, which clears `last_telemetry` because that telemetry
            // described the session being left. The per-turn fields have nothing
            // to report — but the session's own totals do, and those survive the
            // resume (the record carries them). Leaving them in the `Some` arm
            // alone made a resumed conversation read as if it had never spent
            // anything.
            None => format!("\nlast_turn=none{}", self.session_spend_lines()),
        };
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
            if web_enabled() {
                format!("on ({})", self.web_backend)
            } else {
                "off".to_string()
            },
            self.configured_model,
            self.configured_reasoning,
            self.history.len(),
            telemetry
        )));
        self.status = self.tr(TextId::StatusShown).to_string();
    }

    /// The session-scoped half of `/status`, for a session where no turn has run
    /// yet.
    ///
    /// Everything here comes from the runtime's own session totals, which are
    /// restored from the session record — so a `/resume`d conversation reports
    /// the money it has already spent. The per-turn fields (`turn_cost`,
    /// `route`, `context`, …) genuinely have nothing to say without a turn, and
    /// stay absent.
    fn session_spend_lines(&self) -> String {
        // Outside a runtime (tests) there is nothing to read; the block simply
        // does not appear, which is the pre-existing shape.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return String::new();
        };
        let runtime = std::sync::Arc::clone(&self.runtime);
        let spend = tokio::task::block_in_place(|| handle.block_on(runtime.session_spend()));
        let hit = u32::try_from(spend.cache_hit_tokens).unwrap_or(u32::MAX);
        let miss = u32::try_from(spend.cache_miss_tokens).unwrap_or(u32::MAX);
        let session_cache =
            cache_hit_percent(hit, miss).map_or_else(|| "—".to_string(), |pct| format!("{pct}%"));
        let cache_line = self.cache_hit_line("—", &session_cache, hit, miss, spend.cache_savings);
        format!(
            "\nsession_cost={}\n{cache_line}",
            spend.cost.format(self.cost_currency)
        )
    }

    /// One shape for the `/status` cache row, whoever holds the numbers: the
    /// per-turn block (both rates) and the no-turn fallback (session only, with
    /// `turn` reading `—`). Sharing it is what keeps the two from drifting into
    /// differently-worded lines describing the same three facts.
    fn cache_hit_line(
        &self,
        turn: &str,
        session: &str,
        hit: u32,
        miss: u32,
        saved: CostEstimate,
    ) -> String {
        self.tr_with(
            TextId::StatusCacheHitLine,
            &[
                ("turn", turn),
                ("session", session),
                // The raw counts, matching `saved`'s scope: a percentage alone
                // hides where the money went — at a 99% hit rate the remaining
                // 1% is still about a third of the input bill, because the
                // price gap is 50–120x.
                ("hit", &compact_token_count(hit)),
                ("miss", &compact_token_count(miss)),
                ("saved", &saved.format(self.cost_currency)),
            ],
        )
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
                    &[("running", &running.to_string())],
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
                    let note =
                        format_sessions_storage_note(&self.workspace, &crate::cli::program_name());
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
        // Same busy guard as the other state-changing commands: restoring while
        // a turn streams (or is parked on approval) rewinds the workspace under
        // the live turn, which keeps writing on top of it. The runtime refuses
        // it too, but stopping here keeps the transcript honest (no "restored"
        // cell above still-streaming output).
        if self.runtime_busy() {
            self.status = self.tr(TextId::BusyRestore).to_string();
            return;
        }
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
            Ok(kept) => {
                self.last_checkpoint = Some(id.to_string());
                self.apply_runtime_event(RuntimeEvent::WorkspaceRestored { id: checkpoint_id });
                // Checkpoints snapshot the primary workspace only. Restoring
                // while extra roots are granted must say so, or "restored"
                // quietly overpromises about trees the snapshot never held.
                if !self.extra_roots.is_empty() {
                    self.history.push(HistoryCell::system(
                        self.tr(TextId::RestoreExtraRootsNotCovered).to_string(),
                    ));
                }
                // "Restored" was said flat out while `clear` could deliberately
                // leave entries behind (a skipped directory, a link this
                // platform cannot recreate, a FIFO/socket with no snapshot
                // representation). Naming them is the difference between
                // "restored" and "restored, except for these".
                if !kept.is_empty() {
                    self.history.push(HistoryCell::system(self.tr_with(
                        TextId::SystemRestoreKept,
                        &[
                            ("count", &kept.len().to_string()),
                            ("paths", &kept.join(", ")),
                        ],
                    )));
                }
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

/// A token count in the compact form the status line needs: `47.6M`, `820k`,
/// `912`.
///
/// Raw counts run to eight digits on a cached session and would push the cost —
/// the reason the line exists — off the right edge. The exact figures stay
/// re-derivable from the two rates, so this row is a readout, not an audit.
pub(crate) fn compact_token_count(tokens: u32) -> String {
    if tokens < 10_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.0}k", f64::from(tokens) / 1_000.0)
    } else {
        format!("{:.1}M", f64::from(tokens) / 1_000_000.0)
    }
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

    /// The status row has to stay one line, and a cached session's counts reach
    /// eight digits.
    #[test]
    fn compact_token_count_keeps_the_row_one_line() {
        assert_eq!(compact_token_count(0), "0");
        assert_eq!(compact_token_count(912), "912");
        assert_eq!(compact_token_count(9_999), "9999");
        assert_eq!(compact_token_count(10_000), "10k");
        assert_eq!(compact_token_count(820_400), "820k");
        assert_eq!(compact_token_count(1_000_000), "1.0M");
        // The count that made this necessary: `已省 ¥46.69` was the whole row
        // before, and 47_637_857 alone is longer than everything else on it.
        assert_eq!(compact_token_count(47_637_857), "47.6M");
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

    /// Pins the WIRING, which the helper-level test below cannot.
    ///
    /// `copy_last_response_sanitizes_without_flattening_code` calls
    /// `sanitize_for_clipboard` directly, so deleting the call inside
    /// `copy_last_response` left all 200 tests green — the fix was live but
    /// unpinned, the same species of false green this round found in three
    /// other places.
    ///
    /// The observable was already there: the status line counts
    /// `text.chars()` AFTER the shadowing `let`, so a four-char payload
    /// carrying one stripped code point must report three.
    #[test]
    fn copy_last_response_actually_runs_the_text_through_the_sanitizer() {
        let mut app = App::new();
        app.history.push(HistoryCell::assistant("ab\u{202e}c"));
        app.copy_last_response();
        assert!(
            app.status.contains('3'),
            "expected the sanitized length 3, got status {:?}",
            app.status
        );
        assert!(
            !app.status.contains('4'),
            "the raw length reached the status line, so nothing sanitized the \
             text on its way to the clipboard: {:?}",
            app.status
        );
    }

    /// The clipboard is a terminal-bound surface too: pasted `\x1b` repaints,
    /// pasted `\r` can submit. Drag-select copy was already safe because it
    /// reads the sanitized frame; `/copy` read the raw cell text, so the two
    /// copy paths in the same app disagreed.
    ///
    /// Structure has to survive though — `\n` and `\t` are the code block the
    /// user is copying, not stray bytes on a rendered row.
    #[test]
    fn copy_last_response_sanitizes_without_flattening_code() {
        let sanitized = crate::ui::render::sanitize_for_clipboard(
            "fn main() {\n\tlet x = 1;\u{1b}[2K\r\u{202e}\n}",
        );
        assert_eq!(sanitized, "fn main() {\n\tlet x = 1; [2K \n}");
        assert!(
            !sanitized.chars().any(|ch| ch == '\u{1b}' || ch == '\r'),
            "an escape or carriage return reached the clipboard: {sanitized:?}"
        );
        // Indentation and line structure intact.
        assert!(sanitized.contains("\n\tlet x"));
    }
}
