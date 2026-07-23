use super::*;

impl App {
    pub(crate) fn adopt_runtime(&mut self, launched: LaunchedRuntime) {
        for warning in &launched.warnings {
            self.history.push(crate::history::HistoryCell::system(
                self.tr_with(TextId::SystemWarning, &[("message", warning)]),
            ));
        }
        self.runtime = launched.handle;
        self.backend_label = launched.backend_label;
        self.backend_offline = launched.offline;
        self.session_id = launched.session_id;
        self.subagent_manager = launched.subagent_manager;
        self.subagent_shutdown = Some(launched.stop_hook);
    }

    /// Load the layered agent config the same way for every runtime swap:
    /// global (so an `/apikey`-saved key survives) + project
    /// `.deep-code/config.toml` + env. Routing all three swap paths through one
    /// loader keeps `/resume`, `/clear`, and `/model` from drifting — a plain
    /// `AgentConfig::load` here once dropped the saved key on `/resume`.
    fn load_layered_config(&self) -> AgentConfig {
        let project = self.workspace.join(".deep-code").join("config.toml");
        AgentConfig::load_with(
            Some(self.global_config_path.clone()),
            Some(project),
            &|name| std::env::var(name).ok(),
        )
        .config
    }

    /// Stop the subagent supervisor and flush + tear down the live runtime.
    /// Every runtime swap calls this first so the old session's persistence
    /// lands on disk before anything re-reads it.
    fn shutdown_current_runtime(&mut self) {
        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            tokio::task::block_in_place(|| handle.block_on(runtime.shutdown()));
        }
    }

    /// Launch a fresh runtime from the current layered config (optionally
    /// resuming `resume`) and adopt it, carrying the config-derived display
    /// state. The caller must have shut the previous runtime down first.
    fn launch_and_adopt(&mut self, resume: Option<SessionRecord>, resumed: bool) {
        let agent_config = self.load_layered_config();
        let launched = launch_runtime(&agent_config, self.workspace.clone(), resume);
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        // `lang` is deliberately NOT re-resolved here: it changes only at
        // launch and via /lang, so a runtime swap can never flip the UI
        // language out from under the user (or the tests).
        self.resumed = resumed;
        self.adopt_runtime(launched);
    }

    /// Rebuild the runtime with the (re-loaded) layered config, resuming the
    /// current persisted session so the conversation continues seamlessly.
    /// The old runtime is shut down first so its persistence flush lands
    /// before the session record is re-read.
    pub(crate) fn relaunch_runtime(&mut self) -> Result<(), String> {
        if self.is_streaming || self.pending_approval.is_some() {
            return Err(self.tr(TextId::BusyRelaunchConfig).to_string());
        }

        // Flush the old runtime first so re-reading the current session below
        // sees its latest writes.
        self.shutdown_current_runtime();

        let resume = self.session_id.as_ref().and_then(|id| {
            let store = JsonSessionStore::for_workspace(self.workspace.clone()).ok()?;
            let session_id = deep_code_agent::SessionId::parse(id).ok()?;
            store.load(&session_id).ok()
        });
        if resume.is_none() && self.session_id.is_some() {
            // The old runtime is already down; relaunch a fresh session so the
            // app stays usable instead of pointing at a dead runtime.
            self.launch_and_adopt(None, false);
            return Err(self.tr(TextId::SessionReloadFailedRestart).to_string());
        }

        let resumed = resume.is_some();
        self.launch_and_adopt(resume, resumed);
        Ok(())
    }

    pub(crate) fn resume_picker_open(&self) -> bool {
        self.resume_picker.is_some()
    }

    /// Open the in-app `/resume` modal, listing the workspace's non-empty
    /// sessions (newest-first). No-ops with a status note when none qualify.
    pub(crate) fn open_resume_picker(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            self.status = self.tr(TextId::BusySwitchSession).to_string();
            return;
        }
        let sessions = match JsonSessionStore::for_workspace(self.workspace.clone())
            .and_then(|store| store.list())
        {
            Ok(list) => list
                .into_iter()
                .filter(crate::startup::has_user_message)
                .collect::<Vec<_>>(),
            Err(error) => {
                self.status =
                    self.tr_with(TextId::SessionsReadFailed, &[("error", &error.to_string())]);
                return;
            }
        };
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

    /// Load session `id` and switch to it in place. Surfaces a readable status
    /// on a bad id / missing record rather than failing the command.
    pub(crate) fn switch_session_by_id(&mut self, id: &str) -> Result<(), String> {
        let store = JsonSessionStore::for_workspace(&self.workspace).map_err(|error| {
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
        let record = store.load(&session_id).map_err(|error| {
            self.tr_with(
                TextId::SessionNotFound,
                &[("id", id), ("error", &error.to_string())],
            )
        })?;
        self.switch_session(record)
    }

    /// Switch the live session to `record` in place: shut the current runtime
    /// down (flushing its persistence), relaunch resuming `record`, and rebuild
    /// the visible transcript. This is what `/resume` executes.
    pub(crate) fn switch_session(&mut self, record: SessionRecord) -> Result<(), String> {
        if self.is_streaming || self.pending_approval.is_some() {
            return Err(self.tr(TextId::BusySwitchSession).to_string());
        }
        if self.session_id.as_deref() == Some(record.id.as_str()) {
            self.status = self.tr(TextId::StatusAlreadyCurrent).to_string();
            return Ok(());
        }

        self.shutdown_current_runtime();
        self.launch_and_adopt(Some(record.clone()), true);

        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.error = None;
        self.history.extend(hydrate_history(&record));
        self.status = self.tr_with(
            TextId::StatusSwitchedSession,
            &[("id", record.id.as_str()), ("backend", &self.backend_label)],
        );
        Ok(())
    }

    /// Start a fresh conversation in place — what `/clear` executes. The current
    /// session is flushed to disk (recoverable via `/resume`), a brand-new
    /// session is launched, and the view resets to the welcome header.
    pub(crate) fn start_new_conversation(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            self.status = self.tr(TextId::BusyNewConversation).to_string();
            return;
        }

        let workspace_display = home_relative(&self.workspace);
        self.shutdown_current_runtime();
        self.launch_and_adopt(None, false);

        let persistent = self.session_id.is_some();
        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.close_completion();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.last_checkpoint = None;
        self.error = None;
        let cell = welcome_cell(
            &self.configured_model,
            &self.configured_reasoning,
            self.backend_offline,
            workspace_display,
            None,
            persistent,
        );
        self.history.push(cell);
        self.status = self.tr(TextId::StatusNewConversation).to_string();
    }
}
