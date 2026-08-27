use super::*;

impl App {
    pub(crate) fn adopt_runtime(&mut self, launched: LaunchedRuntime) {
        // Exhaustive destructuring for the same reason `App::launch` uses it:
        // a field added to `LaunchedRuntime` must be given a home in BOTH
        // consumers, and skipping one has to be written down as `field: _`.
        // Field-by-field moves are how the startup path came to ignore
        // `warnings` for its whole life without anyone noticing.
        let LaunchedRuntime {
            handle,
            backend_label,
            session_id,
            subagent_manager,
            job_store,
            stop_hook,
            offline,
            warnings,
            permission_mode,
            extra_roots,
        } = launched;
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
        self.permission_mode.set(previous_mode);
    }

    /// Drain the warnings parked by [`Self::adopt_runtime`] into the visible
    /// transcript. Called once per swap, at the point where the transcript is
    /// finished being rebuilt, so a startup degradation is never written
    /// somewhere a subsequent `history.clear()` will erase it.
    pub(crate) fn flush_launch_warnings(&mut self) {
        for warning in std::mem::take(&mut self.pending_launch_warnings) {
            self.history.push(crate::history::HistoryCell::system(
                self.tr_with(TextId::SystemWarning, &[("message", &warning)]),
            ));
        }
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
        // Kill the outgoing session's background jobs and their process tree
        // before the swap, so a dev server from the old session doesn't linger.
        if let Some(job_store) = self.job_store.take() {
            job_store.shutdown();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            tokio::task::block_in_place(|| handle.block_on(runtime.shutdown()));
        }
    }

    /// Launch a fresh runtime from the current layered config (optionally
    /// resuming `resume`) and adopt it, carrying the config-derived display
    /// state. The caller must have shut the previous runtime down first.
    fn launch_and_adopt(&mut self, resume: Option<SessionRecord>) {
        let agent_config = self.load_layered_config();
        // Grants are process-scoped: the human expressed them for this run
        // (launch flags or `/add-dir`), so every swap re-passes the current
        // set. A `/clear` session is born with them, and a `/resume` unions
        // them into the target record — the same semantics `-c --add-dir`
        // already has. Dropping them on a swap instead would deny the model
        // mid-task on paths it legitimately wrote a moment earlier.
        let launched = launch_runtime(
            &agent_config,
            deep_code_agent::WorkspaceRoots::new(self.workspace.clone(), self.extra_roots.clone()),
            resume,
        );
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        // `lang` is deliberately NOT re-resolved here: it changes only at
        // launch and via /lang, so a runtime swap can never flip the UI
        // language out from under the user (or the tests).
        self.adopt_runtime(launched);
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
    pub(crate) fn add_dir_command(&mut self, raw: &str) {
        if self.is_streaming || self.pending_approval.is_some() {
            self.status = self.tr(TextId::BusyRelaunchConfig).to_string();
            return;
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            self.status = self.tr(TextId::AddDirUsage).to_string();
            return;
        }
        // Applying a grant means relaunching into the current record; a
        // session that never persisted has no record to resume, and the
        // relaunch would silently drop the whole conversation. Refuse and
        // name the working alternative instead.
        if self.session_id.is_none() {
            self.status = self.tr(TextId::AddDirNeedsPersistence).to_string();
            return;
        }
        let candidate = std::path::Path::new(trimmed);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace.join(candidate)
        };
        let canonical = match absolute.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                self.status = self.tr_with(
                    TextId::AddDirResolveFailed,
                    &[("path", trimmed), ("error", &error.to_string())],
                );
                return;
            }
        };
        if !canonical.is_dir() {
            self.status = self.tr_with(TextId::AddDirNotDirectory, &[("path", trimmed)]);
            return;
        }
        if self.workspace.canonicalize().ok().as_ref() == Some(&canonical) {
            self.status = self.tr_with(TextId::AddDirAlreadyWorkspace, &[("path", trimmed)]);
            return;
        }
        if self.extra_roots.contains(&canonical) {
            self.status = self.tr_with(TextId::AddDirAlreadyGranted, &[("path", trimmed)]);
            return;
        }
        self.extra_roots.push(canonical.clone());
        match self.relaunch_runtime() {
            Ok(()) => {
                self.push_extra_roots_banner();
                self.status = self.tr_with(
                    TextId::AddDirGranted,
                    &[("dir", &canonical.display().to_string())],
                );
            }
            // The fallback runtime (fresh session) was still adopted with the
            // grant, so the boundary is what the user asked for; surface why
            // the conversation view may have reset instead of claiming success.
            Err(message) => self.status = message,
        }
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
            self.launch_and_adopt(None);
            self.flush_launch_warnings();
            return Err(self.tr(TextId::SessionReloadFailedRestart).to_string());
        }

        self.launch_and_adopt(resume);
        // No rebuild on this path (`/model`, `/apikey`, `/add-dir` keep the
        // transcript), so the warnings belong at the end of it, immediately.
        self.flush_launch_warnings();
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
        self.launch_and_adopt(Some(record.clone()));

        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.error = None;
        // Conversation-scoped, like `history`: a queue must never survive into a
        // different session and auto-send there.
        self.steering_queue.clear();
        self.pending_steering_flush = false;
        self.history.extend(hydrate_history(&record));
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
        self.launch_and_adopt(None);

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
