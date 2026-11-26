use super::*;
use deep_code_agent::{GlobalConfigUpdate, LoadedAgentConfig, write_global_config_update};

#[cfg(test)]
mod switch_tests;
#[cfg(test)]
pub(super) mod tests;

fn run_runtime_future<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
            .map(|runtime| runtime.block_on(future)),
    }
}

pub(super) enum SessionScanPurpose {
    Picker,
    List,
    Resume(deep_code_agent::SessionId),
}

pub(super) struct PendingSessionScan {
    purpose: SessionScanPurpose,
    task: crate::session_scan::SessionScan,
}

pub(super) struct PendingSessionSwitch {
    current: Arc<AgentRuntime>,
    task: crate::session_switch::SessionSwitch,
}

impl PendingSessionSwitch {
    pub(super) fn cancel(&self) {
        self.task.cancel();
    }
}

/// Offer the model's thinking levels and preselect the current effort when
/// supported. Otherwise use the catalog default.
fn picker_efforts(
    registry: &deep_code_agent::ModelRegistry,
    model: &str,
    current_effort: &str,
) -> (Vec<String>, usize) {
    let info = registry
        .info_for(model)
        .expect("picker models are in the catalog");
    let mut efforts = vec!["none".to_string()];
    for level in &info.reasoning_efforts {
        if !efforts.contains(level) {
            efforts.push(level.clone());
        }
    }
    let selected = efforts
        .iter()
        .position(|effort| effort == current_effort)
        .or_else(|| {
            efforts
                .iter()
                .position(|effort| effort == &info.default_reasoning_effort)
        })
        .unwrap_or(0);
    (efforts, selected)
}

impl PendingSessionScan {
    pub(super) fn cancel(&self) {
        self.task.cancel();
    }
}

impl App {
    pub(crate) fn adopt_runtime(&mut self, launched: LaunchedRuntime) -> Option<SessionRecord> {
        // Exhaustive destructuring for the same reason `App::launch` uses it:
        // a field added to `LaunchedRuntime` must be given a home in BOTH
        // consumers, and skipping one has to be written down as `field: _`.
        // Field-by-field moves are how the startup path came to ignore
        // `warnings` for its whole life without anyone noticing.
        let LaunchedRuntime {
            handle,
            backend_label,
            session_id,
            resume_record,
            subagent_manager,
            job_store,
            stop_hook,
            offline,
            warnings,
            permission_mode,
            sandbox_off,
            extra_roots,
            instructions,
            skills,
        } = launched;
        self.instructions = instructions;
        self.skills = skills;
        // Parked, not pushed. Two of the three callers rebuild the transcript
        // AFTER adopting — `switch_session` clears then hydrates, and
        // `start_new_conversation` clears then writes the welcome cell — so
        // pushing here meant the warnings were written and then immediately
        // thrown away. `/resume` is exactly the path that produces the
        // security-relevant ones ("dropping N write grant(s): they carry no
        // valid authorship tag", "dropping recorded grant X: it now resolves
        // to Y"), and the user saw the new boundary banner with nothing
        // explaining why grants had vanished.
        //
        // The exhaustive destructuring above catches a field never READ; it
        // cannot catch one read and then discarded. Each caller now decides
        // where the warnings land via `flush_launch_warnings`.
        self.pending_launch_warnings = warnings;
        // The permission warning was shown for the outgoing runtime.
        self.cancel_yolo_confirmation();
        self.discard_image_work();
        self.runtime = handle;
        self.backend_label = backend_label;
        self.backend_offline = offline;
        self.session_id = session_id;
        self.subagent_manager = subagent_manager;
        self.subagent_shutdown = Some(stop_hook);
        self.job_store = Some(job_store);
        // Track the adopted runtime's effective grants, not the ones this App
        // started with: a swap can change them (`/resume` into a session whose
        // record carries grants, `/add-dir`), and every display surface that
        // reads this field — the `/restore` honesty note, the grants banner —
        // must describe the boundary the live runtime actually enforces.
        self.extra_roots = extra_roots;
        // Carry the user's chosen permission mode onto the new runtime's shared
        // handle so a config swap (/model, /apikey, /resume, /clear) doesn't
        // silently reset it.
        let previous_mode = self.permission_mode.get();
        self.permission_mode = permission_mode;
        // Adopted, not inherited from the previous session: `/resume` and
        // `switch_session` can cross between a config that had `mode = "off"`
        // and one that did not, and a stale chip would either hide a real
        // removal of confinement or cry wolf about one that is gone.
        self.sandbox_off = sandbox_off;
        self.permission_mode.set(previous_mode);
        resume_record
    }

    /// Drain the warnings parked by [`Self::adopt_runtime`] into the visible
    /// transcript. Called once per swap, at the point where the transcript is
    /// finished being rebuilt, so a startup degradation is never written
    /// somewhere a subsequent `history.clear()` will erase it.
    /// A degradation that is still true after a swap is still the same
    /// sentence, so it is delivered once and not once per swap. `/model`,
    /// `/apikey`, `/logout` and `/add-dir` all relaunch WITHOUT clearing the
    /// transcript, and a persistent cause (no session store, tool group
    /// unavailable, checkpoints off) re-parks the identical five lines each
    /// time — three relaunches used to leave four copies of the same block.
    pub(crate) fn flush_launch_warnings(&mut self) {
        for warning in std::mem::take(&mut self.pending_launch_warnings) {
            let rendered = self.tr_with(TextId::SystemWarning, &[("message", &warning)]);
            let already_said = self.history.iter().any(|cell| {
                matches!(cell, crate::history::HistoryCell::System { text } if *text == rendered)
            });
            if already_said {
                continue;
            }
            self.history
                .push(crate::history::HistoryCell::system(rendered));
        }
    }

    /// Load the layered agent config the same way for every runtime swap:
    /// global (so an `/apikey`-saved key survives) + project
    /// `.deep-code/config.toml` + env. Routing all three swap paths through one
    /// loader keeps `/resume`, `/clear`, and `/model` from drifting — a plain
    /// `AgentConfig::load` here once dropped the saved key on `/resume`.
    fn load_layered_config(&self) -> LoadedAgentConfig {
        let project = self.workspace.join(".deep-code").join("config.toml");
        AgentConfig::load_with(self.global_config_path.clone(), Some(project), &|name| {
            std::env::var(name).ok()
        })
    }

    /// Stop the subagent supervisor and flush + tear down the live runtime.
    /// Configuration changes and new conversations use this path; resumed
    /// sessions prepare their flush and launch in a background switch.
    fn shutdown_current_runtime(&mut self) {
        let _ = run_runtime_future(self.stop_current_runtime());
    }

    async fn stop_current_runtime(&mut self) {
        self.session_scan = None;
        self.session_switch = None;
        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        // Kill the outgoing session's background jobs and their process tree
        // before the swap, so a dev server from the old session doesn't linger.
        self.job_store = None;
        self.runtime.shutdown().await;
    }

    /// Launch a fresh runtime from the current layered config (optionally
    /// resuming `resume`) and adopt it, carrying the config-derived display
    /// state. The caller must have shut the previous runtime down first.
    fn launch_and_adopt(
        &mut self,
        agent_config: &AgentConfig,
        resume: Option<SessionRecord>,
    ) -> Result<Option<SessionRecord>, String> {
        deep_code_agent::validate_pinned_workspace(&self.workspace)
            .map_err(|error| error.to_string())?;
        // Preserve unchanged process grants across swaps, but keep their
        // provenance: an old path is not fresh user intent to authorize
        // whatever directory it might now resolve to.
        let launched = deep_code_agent::relaunch_runtime(
            agent_config,
            deep_code_agent::WorkspaceRoots::new(self.workspace.clone(), self.extra_roots.clone()),
            resume,
        )
        .map_err(|error| error.to_string())?;
        Ok(self.adopt_configured_runtime(launched, agent_config))
    }

    fn adopt_configured_runtime(
        &mut self,
        launched: LaunchedRuntime,
        agent_config: &AgentConfig,
    ) -> Option<SessionRecord> {
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        self.web_backend = deep_code_agent::WebBackend::from_config(agent_config).id();
        // `[ui] notifications` is read here for the same reason as the three
        // above: it is a config value, and a swap that forgot it left the old
        // answer in place until the process restarted. (`lang` is the
        // deliberate exception, right below.)
        self.notifications_enabled = agent_config.notifications;
        // `lang` is deliberately NOT re-resolved here: it changes only at
        // launch and via /lang, so a runtime swap can never flip the UI
        // language out from under the user (or the tests). The new runtime
        // is handed the same language, or its own text — errors, approval
        // previews, compaction warnings — reverts to whatever the config said
        // while the screen around it stays in the user's choice: a `/lang`
        // that could not be saved, an environment override.
        let effective_resume = self.adopt_runtime(launched);
        self.runtime.set_ui_lang(self.lang);
        effective_resume
    }

    /// Append the effective-grants line to the transcript. Called wherever the
    /// visible history is rebuilt or the grant set changes — an invisible
    /// write boundary is indistinguishable from a bug when a path outside it
    /// is denied, or quietly accepted.
    pub(crate) fn push_extra_roots_banner(&mut self) {
        if self.extra_roots.is_empty() {
            return;
        }
        let dirs = self
            .extra_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.history.push(crate::history::HistoryCell::system(
            self.tr_with(TextId::ExtraRootsGrantedLabel, &[("dirs", &dirs)]),
        ));
    }

    /// `/add-dir DIR` — grant an extra writable root mid-session. Mirrors the
    /// CLI flag: the path is canonicalized at the moment the human states the
    /// intent, then applied by relaunching the runtime into the current
    /// session, which is the same union → persist → prompt-rebuild path that
    /// `-c --add-dir` exercises. Slash commands can only be typed at the
    /// keyboard, so this opens no model-reachable widening channel.
    ///
    /// "Mirrors the CLI flag" has to include WHICH `canonicalize`, and for one
    /// release it did not: this twin resolved with `Path::canonicalize` while
    /// the flag had already been corrected to the agent crate's reading. The
    /// resolution now lives in one body,
    /// [`crate::cli::resolve_grant_dir`], whose doc carries the cost of the
    /// difference; the workspace comparison below uses the same reading for
    /// the same reason, or under two spellings it misses and the workspace
    /// gets recorded as its own "additional" root.
    pub(crate) fn add_dir_command(&mut self, raw: &str) {
        if self.runtime_busy() {
            self.status = self.busy_status(TextId::BusyRelaunchConfig);
            return;
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            self.status = self.tr(TextId::AddDirUsage).to_string();
            return;
        }
        let candidate = std::path::Path::new(trimmed);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace.join(candidate)
        };
        let canonical = match crate::cli::resolve_grant_dir(&absolute) {
            Ok(path) => path,
            Err(crate::cli::GrantDirError::Unresolvable(error)) => {
                self.status = self.tr_with(
                    TextId::AddDirResolveFailed,
                    &[("path", trimmed), ("error", &error.to_string())],
                );
                return;
            }
            Err(crate::cli::GrantDirError::NotADirectory) => {
                self.status = self.tr_with(TextId::AddDirNotDirectory, &[("path", trimmed)]);
                return;
            }
        };
        if deep_code_agent::canonicalize(&self.workspace).ok().as_ref() == Some(&canonical) {
            self.status = self.tr_with(TextId::AddDirAlreadyWorkspace, &[("path", trimmed)]);
            return;
        }
        if self.extra_roots.contains(&canonical) {
            self.status = self.tr_with(TextId::AddDirAlreadyGranted, &[("path", trimmed)]);
            return;
        }
        self.extra_roots.push(canonical.clone());
        match self.relaunch_runtime() {
            Ok(_) => {
                self.push_extra_roots_banner();
                self.status = self.tr_with(
                    TextId::AddDirGranted,
                    &[("dir", &canonical.display().to_string())],
                );
            }
            Err(message) => {
                self.extra_roots.retain(|root| *root != canonical);
                self.status = message;
            }
        }
    }

    /// Prepare the replacement from live memory before retiring the old runtime.
    pub(crate) fn relaunch_runtime(&mut self) -> Result<LoadedAgentConfig, String> {
        if self.runtime_busy() {
            return Err(self.busy_status(TextId::BusyRelaunchConfig));
        }

        deep_code_agent::validate_pinned_workspace(&self.workspace)
            .map_err(|error| error.to_string())?;
        let loaded = self.load_layered_config();
        let agent_config = &loaded.config;
        run_runtime_future(async {
            let snapshot = self
                .runtime
                .live_snapshot()
                .await
                .map_err(|error| error.to_string())?;
            let launched = deep_code_agent::prepare_runtime_reconfiguration(
                agent_config,
                deep_code_agent::WorkspaceRoots::new(
                    self.workspace.clone(),
                    self.extra_roots.clone(),
                ),
                snapshot,
            )
            .map_err(|error| error.to_string())?;
            self.stop_current_runtime().await;
            self.adopt_configured_runtime(launched, agent_config);
            if let Some(error) = self.runtime.flush_session().await {
                self.pending_launch_warnings
                    .push(format!("failed to persist reconfigured session: {error}"));
            }
            Ok::<(), String>(())
        })??;
        // No rebuild on this path (`/model`, `/apikey`, `/add-dir` keep the
        // transcript), so the warnings belong at the end of it, immediately.
        self.flush_launch_warnings();
        Ok(loaded)
    }

    pub(crate) fn save_model_settings(&mut self, model: String, reasoning_effort: Option<String>) {
        if self.runtime_busy() {
            self.status = self.busy_status(TextId::BusyRelaunchConfig);
            return;
        }
        let Some(config_path) = self.global_config_path.as_deref() else {
            self.status = self.tr(TextId::GlobalConfigUnavailable).to_string();
            return;
        };
        let (update, saved) = match reasoning_effort {
            Some(reasoning_effort) => {
                let saved = self.tr_with(
                    TextId::GlobalModelAndReasoningSaved,
                    &[("model", &model), ("reasoning", &reasoning_effort)],
                );
                (
                    GlobalConfigUpdate::ModelAndReasoning {
                        model,
                        reasoning_effort,
                    },
                    saved,
                )
            }
            None => {
                let saved = self.tr_with(TextId::GlobalModelSaved, &[("model", &model)]);
                (GlobalConfigUpdate::Model(model), saved)
            }
        };
        match write_global_config_update(config_path, &update, self.lang)
            .and_then(|_| self.relaunch_runtime())
        {
            Ok(loaded) => {
                let model = loaded
                    .config
                    .resolved_model()
                    .expect("runtime reconfiguration validates the model");
                let sources = loaded.report.sources;
                self.status = self.tr_with(
                    TextId::ActiveModelSettings,
                    &[
                        ("model", model),
                        ("model_source", sources.model.localized_label(self.lang)),
                        ("reasoning", loaded.config.reasoning_effort.as_setting()),
                        (
                            "reasoning_source",
                            sources.reasoning_effort.localized_label(self.lang),
                        ),
                        ("backend", &self.backend_label),
                    ],
                );
                self.history
                    .push(HistoryCell::system(format!("{saved} {}", self.status)));
            }
            Err(message) => self.status = message,
        }
    }

    pub(crate) fn resume_picker_open(&self) -> bool {
        self.resume_picker.is_some()
    }

    /// Whether the model/reasoning picker owns the terminal input.
    pub(crate) fn model_picker_open(&self) -> bool {
        self.model_picker.is_some()
    }

    /// Open the two-step model + reasoning picker. The values stay in the
    /// inline menus until the second Enter, so changing both settings causes
    /// one config write and one runtime relaunch.
    pub(crate) fn open_model_picker(&mut self) {
        if self.runtime_busy() {
            self.status = self.busy_status(TextId::BusyRelaunchConfig);
            return;
        }

        let registry = deep_code_agent::ModelRegistry::default();
        let models: Vec<_> = registry
            .list()
            .iter()
            .map(|model| model.id.clone())
            .collect();
        let current_model = match registry.resolve(Some(&self.configured_model)) {
            Ok(model) => model,
            Err(error) => {
                self.status = error.user_message(self.lang);
                return;
            }
        };
        let selected_model = models
            .iter()
            .position(|model| model == current_model)
            .unwrap_or(0);

        let (efforts, selected_effort) =
            picker_efforts(&registry, current_model, &self.configured_reasoning);

        self.close_completion();
        self.clear_selection();
        self.model_picker = Some(ModelPicker {
            models,
            selected_model,
            efforts,
            selected_effort,
            focus: ModelPickerFocus::Model,
        });
        self.completion = Some(self.model_picker_menu(CompletionKind::ModelPicker));
        self.status = self.tr(TextId::ModelPickerHelp).to_string();
    }

    /// Build a picker step using the same inline menu rendered for slash
    /// commands and file completion. The selected value is kept by index in
    /// `ModelPicker`; the menu only owns the current highlight.
    fn model_picker_menu(&self, kind: CompletionKind) -> CompletionMenu {
        let Some(picker) = self.model_picker.as_ref() else {
            return CompletionMenu {
                kind,
                items: Vec::new(),
                selected: 0,
            };
        };
        let (items, selected) = match kind {
            CompletionKind::ModelPicker => (
                picker
                    .models
                    .iter()
                    .map(|model| {
                        let label = match model.as_str() {
                            deep_code_agent::DEEPSEEK_V4_PRO => "Pro".to_string(),
                            deep_code_agent::DEEPSEEK_FLASH => "Flash".to_string(),
                            other => other.to_string(),
                        };
                        (label, model.clone())
                    })
                    .collect(),
                picker.selected_model,
            ),
            CompletionKind::ReasoningPicker => (
                picker
                    .efforts
                    .iter()
                    .map(|effort| {
                        let label = match effort.as_str() {
                            "none" => "None",
                            "low" => "Low",
                            "high" => "High",
                            "max" => "Max",
                            other => other,
                        };
                        (label.to_string(), String::new())
                    })
                    .collect(),
                picker.selected_effort,
            ),
            CompletionKind::Slash | CompletionKind::File => (Vec::new(), 0),
        };
        CompletionMenu {
            kind,
            items,
            selected,
        }
    }

    /// Finish one of the two inline picker steps. Selecting a model replaces
    /// the same menu with the effort choices; selecting an effort commits both
    /// values through the existing atomic config path.
    pub(crate) fn model_picker_accept_selection(&mut self) {
        let Some(menu) = self.completion.as_ref().cloned() else {
            return;
        };
        match menu.kind {
            CompletionKind::ModelPicker => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.selected_model = menu.selected;
                }
                self.model_picker_refresh_efforts();
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.focus = ModelPickerFocus::Reasoning;
                }
                self.completion = Some(self.model_picker_menu(CompletionKind::ReasoningPicker));
            }
            CompletionKind::ReasoningPicker => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.selected_effort = menu.selected;
                    picker.focus = ModelPickerFocus::Reasoning;
                }
                self.close_completion();
                self.model_picker_accept();
            }
            CompletionKind::Slash | CompletionKind::File => {}
        }
    }

    /// Recompute the effort choices after selecting a model, preserving the
    /// current effort when the new model supports it and otherwise falling
    /// back to the catalog default.
    fn model_picker_refresh_efforts(&mut self) {
        let Some((model, previous_effort)) = self.model_picker.as_ref().map(|picker| {
            (
                picker.models.get(picker.selected_model).cloned(),
                picker.efforts.get(picker.selected_effort).cloned(),
            )
        }) else {
            return;
        };
        let Some(model) = model else {
            return;
        };
        let registry = deep_code_agent::ModelRegistry::default();
        let (efforts, selected_effort) = picker_efforts(
            &registry,
            &model,
            previous_effort.as_deref().unwrap_or("high"),
        );
        if let Some(picker) = self.model_picker.as_mut() {
            picker.efforts = efforts;
            picker.selected_effort = selected_effort;
        }
    }

    pub(crate) fn model_picker_cancel(&mut self) {
        self.close_completion();
        if self.model_picker.take().is_some() {
            self.status = self.tr(TextId::StatusModelPickerCancelled).to_string();
        }
    }

    /// Apply both picker rows atomically, preserving the current session.
    pub(crate) fn model_picker_accept(&mut self) {
        let Some(picker) = self.model_picker.take() else {
            return;
        };
        let Some(model) = picker.models.get(picker.selected_model).cloned() else {
            return;
        };
        let Some(reasoning_effort) = picker.efforts.get(picker.selected_effort).cloned() else {
            return;
        };
        self.save_model_settings(model, Some(reasoning_effort));
    }

    /// Open the in-app `/resume` modal, listing the workspace's non-empty
    /// sessions (newest-first). No-ops with a status note when none qualify.
    pub(crate) fn open_resume_picker(&mut self) {
        if self.runtime_busy() {
            self.status = self.busy_status(TextId::BusySwitchSession);
            return;
        }
        self.begin_session_scan(SessionScanPurpose::Picker);
    }

    pub(crate) fn list_sessions(&mut self) {
        self.begin_session_scan(SessionScanPurpose::List);
    }

    fn begin_session_scan(&mut self, purpose: SessionScanPurpose) {
        // A newer request owns a new receiver; a cancelled worker can never
        // deliver a stale result into it.
        self.session_scan = None;
        self.session_switch = None;
        match JsonSessionStore::for_pinned_workspace(&self.workspace)
            .and_then(crate::session_scan::SessionScan::start)
        {
            Ok(task) => {
                self.session_scan = Some(PendingSessionScan { purpose, task });
                self.status = self.tr(TextId::StatusScanningSessions).to_string();
            }
            Err(error) => {
                self.status =
                    self.tr_with(TextId::SessionsReadFailed, &[("error", &error.to_string())]);
            }
        }
    }

    pub(crate) fn cancel_session_scan(&mut self) -> bool {
        let scan = self.session_scan.take();
        let switch = self.session_switch.take();
        if scan.is_some() || switch.is_some() {
            self.status = self.tr(TextId::StatusResumeCancelled).to_string();
            true
        } else {
            false
        }
    }

    /// Nonblocking: the terminal loop still drains keys and runtime events
    /// while the worker reads the directory.
    pub(crate) fn poll_session_scan(&mut self) -> bool {
        let cleanup_changed = self.poll_session_cleanup();
        if self.poll_session_switch() {
            return true;
        }
        let Some(result) = self
            .session_scan
            .as_mut()
            .and_then(|scan| scan.task.try_result())
        else {
            return cleanup_changed;
        };
        let purpose = self
            .session_scan
            .take()
            .expect("scan result has an owner")
            .purpose;
        match result {
            Ok(listing) => {
                if !listing.diagnostics.is_empty() {
                    self.history
                        .push(HistoryCell::system(listing.diagnostics.join("\n")));
                }
                match purpose {
                    SessionScanPurpose::Picker if !self.runtime_busy() => {
                        self.show_resume_picker(listing.records);
                    }
                    SessionScanPurpose::Picker => {
                        self.status = self.busy_status(TextId::BusySwitchSession);
                    }
                    SessionScanPurpose::List => self.show_session_list(listing.records),
                    SessionScanPurpose::Resume(_) => {
                        let record = listing
                            .records
                            .into_iter()
                            .next()
                            .expect("single-session load returned its record");
                        if let Err(message) = self.switch_session(record) {
                            self.status = message;
                        }
                    }
                }
            }
            Err(error) => {
                self.status = match purpose {
                    SessionScanPurpose::Resume(id) => self.tr_with(
                        TextId::SessionNotFound,
                        &[("id", id.as_str()), ("error", &error.to_string())],
                    ),
                    SessionScanPurpose::Picker | SessionScanPurpose::List => {
                        self.tr_with(TextId::SessionsReadFailed, &[("error", &error.to_string())])
                    }
                };
            }
        }
        true
    }

    fn poll_session_switch(&mut self) -> bool {
        let Some(result) = self
            .session_switch
            .as_mut()
            .and_then(|switch| switch.task.try_result())
        else {
            return false;
        };
        let pending = self
            .session_switch
            .take()
            .expect("switch result has an owner");
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status = error;
                return true;
            }
        };
        if !Arc::ptr_eq(&pending.current, &self.runtime)
            || self.runtime_busy()
            || !self.runtime.is_idle()
        {
            self.status = self.busy_status(TextId::BusySwitchSession);
            return true;
        }

        let (launched, config) = prepared.take();
        // Starting the cleanup worker can fail. Do this before retiring the
        // current runtime, and hold its commit permit until adoption succeeds.
        let mut cleanup =
            match crate::session_switch::SessionCleanup::prepare(Arc::clone(&self.runtime)) {
                Ok(cleanup) => cleanup,
                Err(error) => {
                    (launched.stop_hook)();
                    launched.job_store.shutdown();
                    self.status = error;
                    return true;
                }
            };
        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        if let Some(jobs) = self.job_store.take() {
            jobs.shutdown();
        }
        // Everything below is an in-memory commit. No shutdown, save or
        // session-store lock may be awaited on the terminal thread.
        let record = self.adopt_configured_runtime(launched, &config);
        self.finish_session_switch(record.as_ref());
        // Enqueue now, before the UI can request another switch. The worker
        // only waits for this receipt; it cannot issue a stale save later.
        cleanup.commit(self.runtime.commit_prepared_session());
        self.session_cleanup.push(cleanup);
        true
    }

    fn poll_session_cleanup(&mut self) -> bool {
        let mut errors = Vec::new();
        self.session_cleanup
            .retain(|cleanup| match cleanup.try_result() {
                None => true,
                Some(Ok(())) => false,
                Some(Err(error)) => {
                    errors.push(error);
                    false
                }
            });
        if errors.is_empty() {
            return false;
        }
        let error = errors.join("\n");
        if !self.save_error_notified {
            self.history.push(HistoryCell::system(
                self.tr_with(TextId::SystemSaveFailed, &[("error", &error)]),
            ));
        }
        self.save_error_notified = true;
        self.status = self.tr_with(TextId::StatusSaveFailed, &[("error", &error)]);
        true
    }

    fn show_session_list(&mut self, records: Vec<SessionRecord>) {
        if records.is_empty() {
            self.status = self.tr(TextId::NoSavedSessions).to_string();
            return;
        }
        let note = deep_code_agent::format_sessions_storage_note(
            &self.workspace,
            &crate::cli::program_name(),
        );
        let body = records
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
            .join("\n");
        self.history.push(HistoryCell::system(format!(
            "{note}\n{}\n{body}",
            self.tr(TextId::SessionsHeader),
        )));
        self.status = self.tr_with(
            TextId::SessionsCount,
            &[("count", &records.len().to_string())],
        );
    }

    fn show_resume_picker(&mut self, records: Vec<SessionRecord>) {
        let sessions: Vec<_> = records
            .into_iter()
            .filter(crate::startup::has_user_message)
            .collect();
        if sessions.is_empty() {
            self.status = self.tr(TextId::NoResumableSessions).to_string();
            return;
        }
        self.close_completion();
        self.clear_selection();
        self.resume_picker = Some(ResumePicker {
            sessions,
            selected: 0,
        });
        self.status = self.tr(TextId::StatusPickSession).to_string();
    }

    pub(crate) fn resume_picker_up(&mut self) {
        if let Some(picker) = self.resume_picker.as_mut() {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    pub(crate) fn resume_picker_down(&mut self) {
        if let Some(picker) = self.resume_picker.as_mut()
            && picker.selected + 1 < picker.sessions.len()
        {
            picker.selected += 1;
        }
    }

    pub(crate) fn resume_picker_cancel(&mut self) {
        if self.resume_picker.take().is_some() {
            self.status = self.tr(TextId::StatusResumeCancelled).to_string();
        }
    }

    /// Switch to the highlighted session and close the modal.
    pub(crate) fn resume_picker_accept(&mut self) {
        let Some(mut picker) = self.resume_picker.take() else {
            return;
        };
        if picker.selected >= picker.sessions.len() {
            return;
        }
        let record = picker.sessions.swap_remove(picker.selected);
        if let Err(message) = self.switch_session(record) {
            self.status = message;
        }
    }

    /// Start a cancellable background load; polling applies the switch later.
    /// A bad id fails immediately, while a missing record is reported on completion.
    pub(crate) fn switch_session_by_id(&mut self, id: &str) -> Result<(), String> {
        self.cancel_session_scan();
        if self.runtime_busy() {
            return Err(self.busy_status(TextId::BusySwitchSession));
        }
        let store = JsonSessionStore::for_pinned_workspace(&self.workspace).map_err(|error| {
            self.tr_with(
                TextId::SessionStoreOpenFailed,
                &[("error", &error.to_string())],
            )
        })?;
        let session_id = deep_code_agent::SessionId::parse(id).map_err(|error| {
            self.tr_with(
                TextId::SessionIdInvalid,
                &[("id", id), ("error", &error.to_string())],
            )
        })?;
        let task =
            crate::session_scan::SessionScan::load(store, session_id.clone()).map_err(|error| {
                self.tr_with(
                    TextId::SessionNotFound,
                    &[("id", id), ("error", &error.to_string())],
                )
            })?;
        self.session_scan = Some(PendingSessionScan {
            purpose: SessionScanPurpose::Resume(session_id),
            task,
        });
        self.status = self.tr(TextId::StatusScanningSessions).to_string();
        Ok(())
    }

    /// Prepare the entire switch in the background. The old runtime remains
    /// usable until polling commits a completed, still-current request.
    pub(crate) fn switch_session(&mut self, record: SessionRecord) -> Result<(), String> {
        self.cancel_session_scan();
        if self.runtime_busy() {
            return Err(self.busy_status(TextId::BusySwitchSession));
        }
        if self.session_id.as_deref() == Some(record.id.as_str()) {
            self.status = self.tr(TextId::StatusAlreadyCurrent).to_string();
            return Ok(());
        }

        let current = Arc::clone(&self.runtime);
        let task = crate::session_switch::SessionSwitch::start(
            Arc::clone(&current),
            deep_code_agent::WorkspaceRoots::new(self.workspace.clone(), self.extra_roots.clone()),
            self.global_config_path.clone(),
            record,
        )?;
        self.session_switch = Some(PendingSessionSwitch { current, task });
        self.status = self.tr(TextId::StatusSwitchingSession).to_string();
        Ok(())
    }

    fn finish_session_switch(&mut self, effective_record: Option<&SessionRecord>) {
        self.history.clear();
        self.save_error_notified = false;
        self.active_turn = None;
        self.clear_selection();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.error = None;
        // Conversation-scoped, like `history`: a queue must never survive into a
        // different session and auto-send there.
        self.steering_queue.clear();
        self.pending_steering_flush = false;
        if let Some(effective_record) = effective_record {
            self.history.extend(hydrate_history(effective_record));
        }
        // After the rebuild, so the clear above cannot erase them: resuming is
        // the path that drops grants carrying no valid authorship tag, and the
        // banner below would otherwise state a narrowed boundary with nothing
        // saying why.
        self.flush_launch_warnings();
        // The switched-to boundary can differ from the one on screen so far
        // (the record's own grants ∪ this run's); restate it with the rebuilt
        // transcript.
        self.push_extra_roots_banner();
        self.status = self.tr_with(
            TextId::StatusSwitchedSession,
            &[
                ("id", self.session_id.as_deref().unwrap_or("")),
                ("backend", &self.backend_label),
            ],
        );
    }

    /// Start a fresh conversation in place — what `/clear` executes. The current
    /// session is flushed to disk (recoverable via `/resume`), a brand-new
    /// session is launched, and the view resets to the welcome header.
    pub(crate) fn start_new_conversation(&mut self) {
        if self.runtime_busy() {
            self.status = self.busy_status(TextId::BusyNewConversation);
            return;
        }

        if let Err(error) = deep_code_agent::validate_pinned_workspace(&self.workspace) {
            self.status = error.to_string();
            return;
        }
        let agent_config = self.load_layered_config().config;
        if let Err(error) = agent_config.resolved_model() {
            self.status = error.user_message(self.lang);
            return;
        }
        let workspace_display = home_relative(&self.workspace);
        self.shutdown_current_runtime();
        if let Err(error) = self.launch_and_adopt(&agent_config, None) {
            self.status = error;
            return;
        }

        let persistent = self.session_id.is_some();
        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.close_completion();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.last_checkpoint = None;
        self.error = None;
        self.steering_queue.clear();
        self.pending_steering_flush = false;
        let cell = welcome_cell(
            &self.configured_model,
            &self.configured_reasoning,
            self.backend_offline,
            workspace_display,
            None,
            persistent,
            crate::history::instructions_summary(self.lang, &self.instructions),
        );
        self.history.push(cell);
        // After the welcome cell, and after the clear above — which used to
        // erase these before anyone could read them.
        self.flush_launch_warnings();
        // The fresh session inherits this run's grants (see launch_and_adopt);
        // name them so the new transcript starts with the true boundary.
        self.push_extra_roots_banner();
        self.status = self.tr(TextId::StatusNewConversation).to_string();
    }
}
