//! The streaming-turn lifecycle on [`App`]: submitting a prompt (and the
//! steering queue), the bridge that drains `RuntimeEvent`s into the UI, starting
//! and cancelling a turn, and applying each update. Split from `app/mod.rs` as
//! one cohesive concept; the `impl App` methods here are the whole turn/stream
//! concern.

use super::*;
use tokio::sync::oneshot::error::TryRecvError;

pub(super) struct PendingRetry {
    input: String,
    result: images::ImageWorker<Option<VerifiedDraft>>,
}

struct VerifiedDraft {
    entry: Arc<deep_code_agent::SessionEntry>,
    available: Vec<bool>,
}

impl App {
    pub(super) fn typed_slash_command(&self, expanded: &str) -> bool {
        !self.literal_prompt
            && self.input.split_whitespace().next().is_some_and(|typed| {
                typed.starts_with('/') && Some(typed) == expanded.split_whitespace().next()
            })
    }

    pub fn submit(&mut self) {
        // An approval is a modal decision — it must be answered, not typed
        // over — so the composer stays inert until it resolves.
        if self.pending_approval.is_some() {
            return;
        }
        if !self.clipboard_snapshots.is_empty() {
            self.status = self.tr(TextId::ImageSaving).to_owned();
            return;
        }
        self.close_completion();
        // The transcript is about to grow; drop any stale selection.
        self.clear_selection();

        // During editing, the composer shows compact `[粘贴 #N …]` chips so
        // long pasted blocks don't take over the input area.  At submit time
        // both the transcript and the model receive the **expanded** content
        // so the user can see what they actually sent.
        let display = self.input.trim().to_string();
        if display.is_empty() {
            self.status = self.tr(TextId::StatusEmptyPrompt).to_string();
            return;
        }
        let sent = self.expand_pasted(if self.literal_prompt {
            &self.input
        } else {
            &display
        });
        // `/apikey` stops here: it never enters the recallable prompt history,
        // and never reaches the model. Recognised on the text as sent, chips
        // expanded, rather than on the composer's: a key pasted together with
        // its line ending collapses into a `[粘贴 #1 +1 行]` chip, and read off
        // the chip that line was no command at all — it was remembered, and sent
        // like any prompt, into the session transcript with it.
        let as_sent = sent.trim();
        if as_sent.starts_with("/apikey") {
            if self.literal_prompt {
                self.status = self.tr(TextId::RetryApiKey).to_owned();
                return;
            }
            self.handle_slash_command(as_sent);
            self.clear_input();
            return;
        }
        let typed_command = self.typed_slash_command(as_sent);
        if typed_command && (display == "/retry" || display.starts_with("/retry ")) {
            self.edit_unsent(as_sent.strip_prefix("/retry").unwrap_or_default().trim());
            return;
        }
        let images = self.attached_image_inputs();
        // Remember what was typed, with its attachments, before clearing it.
        self.remember_prompt(&sent);

        // `/image` edits the *draft*, not the session, so it cannot go through
        // the branch below: that branch clears the composer after a command
        // runs, which would wipe the very chip this command just inserted. The
        // typed line is cleared first so what is left is only the chip (bare
        // `/image` inserts nothing, and clears the line the same way).
        if typed_command && (display == "/image" || display.starts_with("/image ")) {
            let arg = as_sent
                .strip_prefix("/image")
                .unwrap_or_default()
                .trim()
                .to_string();
            self.clear_input();
            self.handle_image_command(&arg);
            return;
        }

        // Slash commands are interactive directives, not conversation — they
        // run immediately (against the live UI) rather than queueing behind a
        // stream, in both idle and streaming states.
        //
        // Recognised on what was typed but run on what was sent, chips
        // expanded: a focus pasted after `/compact `, a path after `/add-dir `
        // or a query after `/find ` reached the command as its
        // `[粘贴 #1 +5 行]` label otherwise, and the pasted text was dropped
        // with the composer. The command word itself has to be the typed one —
        // a `/` followed by a pasted `clear` is text, not a command a paste
        // assembled.
        if typed_command && self.handle_slash_command(as_sent) {
            self.clear_input();
            return;
        }

        // A compaction is rewriting the history the next turn would be built on:
        // refuse (keeping the draft) rather than race it.
        if self.compacting() {
            self.status = self.tr(TextId::StatusCompacting).to_string();
            return;
        }

        // Steering: a plain prompt typed mid-stream is queued, not dropped.
        // It's sent as a follow-up when the turn ends — the user no longer has
        // to wait for a long turn to finish before lining up the next message.
        // Not added to the transcript here: the streaming turn's own output
        // (held in `active_turn`) hasn't been flushed to `history` yet, so a
        // user cell pushed now would render ABOVE it. The cell is added when
        // the queue flushes, after the current turn's cells land.
        if self.is_streaming {
            // The steering queue carries text and pairs entries with the
            // runtime's queue by FIFO position. Keep an image prompt in the
            // composer for the next turn.
            if !images.is_empty() {
                self.status = self.tr(TextId::ImageWhileStreaming).to_string();
                return;
            }
            if self.steering_queue.len() >= STEERING_QUEUE_CAP {
                // Leave the draft in the composer — losing it is worse than
                // refusing to take more.
                self.status = self.tr_with(
                    TextId::StatusSteeringQueueFull,
                    &[("count", &self.steering_queue.len().to_string())],
                );
                return;
            }
            self.steering_queue.push(sent.clone());
            // Also hand the text to the runtime, so it can steer it into the
            // *live* turn at the next tool-batch boundary rather than making the
            // user wait for the whole turn. The queue above stays the source of
            // truth for what the user typed: it is what the pending UI renders,
            // and what fires as a new turn when the turn ends before any
            // boundary arrives. No double-send — `begin_turn` clears the
            // runtime's copy, so the fallback path below can never race the
            // injected one.
            let runtime = Arc::clone(&self.runtime);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = runtime.steer(sent).await;
                });
            }
            self.clear_input();
            self.status = self.tr_with(
                TextId::StatusSteeringQueued,
                &[("count", &self.steering_queue.len().to_string())],
            );
            return;
        }

        self.clear_input();
        self.start_user_turn(sent, images, None);
    }

    fn edit_unsent(&mut self, arg: &str) {
        let index = if arg.is_empty() {
            Some(0)
        } else {
            arg.parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
        };
        self.error = None;
        let (Some(index), Ok(handle)) = (index, tokio::runtime::Handle::try_current()) else {
            self.status = self.tr(TextId::RetryMissing).to_owned();
            return;
        };
        if self
            .pending_retry
            .as_ref()
            .is_some_and(|pending| pending.input == self.input)
        {
            return;
        }
        let runtime = Arc::clone(&self.runtime);
        match images::spawn_image_worker("retry-draft", move |cancel| {
            let entry = handle
                .block_on(runtime.unsent_messages())
                .into_iter()
                .nth(index)?;
            let deep_code_agent::EntryKind::User { content } = &entry.kind else {
                return None;
            };
            let available = content
                .images()
                .map(|image| image.verify_cancellable(cancel).is_ok())
                .collect();
            Some(VerifiedDraft { entry, available })
        }) {
            Ok(result) => {
                self.pending_retry = Some(PendingRetry {
                    input: self.input.clone(),
                    result,
                });
                self.status = self.tr(TextId::RetryChecking).to_owned();
            }
            Err(error) => {
                self.status =
                    self.tr_with(TextId::RetryCheckFailed, &[("reason", &error.to_string())]);
            }
        }
    }

    pub(super) fn poll_retry(&mut self) -> bool {
        let Some(pending) = &mut self.pending_retry else {
            return false;
        };
        let result = pending.result.try_recv();
        if matches!(result, Err(TryRecvError::Empty)) {
            return false;
        }
        let input_matches = pending.input == self.input;
        self.pending_retry = None;
        if !input_matches {
            self.status = self.tr(TextId::RetryInputChanged).to_owned();
            return true;
        }
        match result {
            Ok(Some(draft)) => self.restore_retry(draft),
            Ok(None) => self.status = self.tr(TextId::RetryMissing).to_owned(),
            Err(_) => {
                self.status = self.tr_with(
                    TextId::RetryCheckFailed,
                    &[("reason", self.tr(TextId::ImageWorkerStopped))],
                )
            }
        }
        true
    }

    fn restore_retry(&mut self, VerifiedDraft { entry, available }: VerifiedDraft) {
        let deep_code_agent::EntryKind::User { content } = &entry.kind else {
            return;
        };
        let Some(unsent) = &entry.unsent else { return };

        let parked = std::mem::take(&mut self.history_draft);
        self.clear_input();
        self.input = content.text();
        let mut missing = unsent.missing_image_labels.clone();
        for chip in &missing {
            self.input = self.input.replace(chip, "");
        }
        for (index, (image, available)) in content.images().zip(available).enumerate() {
            let chip = unsent
                .image_labels
                .get(index)
                .filter(|chip| self.input.contains(chip.as_str()));
            if !available {
                if let Some(chip) = chip.filter(|chip| !chip.is_empty()) {
                    self.input = self.input.replace(chip, "");
                    missing.push(chip.clone());
                } else {
                    missing.push(image.to_string());
                }
                continue;
            }
            let id = chip
                .and_then(|chip| chip.split_once('#'))
                .and_then(|(_, rest)| rest.split_once(' '))
                .and_then(|(id, _)| id.parse::<usize>().ok());
            let (id, chip) = match (id, chip) {
                (Some(id), Some(chip)) => (id, chip.clone()),
                _ => {
                    let id = self.next_image_id + 1;
                    let chip = self.tr_with(
                        TextId::ImageChip,
                        &[("id", &id.to_string()), ("format", image.format.label())],
                    );
                    self.input.push(' ');
                    self.input.push_str(&chip);
                    (id, chip)
                }
            };
            self.next_image_id = self.next_image_id.max(id);
            self.attached_images.push(images::AttachedImage {
                id,
                chip,
                source: deep_code_agent::UserImage::Saved(image.clone()),
                format: image.format,
                bytes: image.bytes,
            });
        }
        self.literal_prompt = !self.input.trim().is_empty();
        self.cursor_to_end();
        if !parked.text.is_empty() {
            self.remember_prompt(&self.input.clone());
            self.history_cursor = Some(self.prompt_history.len() - 1);
            self.history_draft = parked;
        }
        self.status = if missing.is_empty() {
            self.tr(TextId::RetryReady).to_owned()
        } else {
            let warning = self.tr_with(
                TextId::RetryMissingImages,
                &[("images", &missing.join(", "))],
            );
            self.history.push(HistoryCell::system(warning.clone()));
            warning
        };
    }

    /// Start a turn from `text` (plus `images`) right now — the tail of
    /// [`Self::submit`] once the composer and the slash commands are out of the
    /// way, shared with `/init`, which sends a canned brief. `shown` is what
    /// the transcript's user cell displays when that should not be the full
    /// text; the session record always keeps what was actually sent.
    pub(crate) fn start_user_turn(
        &mut self,
        text: String,
        images: Vec<(deep_code_agent::UserImage, String)>,
        shown: Option<String>,
    ) {
        if self
            .session_switch
            .as_ref()
            .is_some_and(|switch| switch.restoring)
        {
            self.cancel_session_scan();
        }
        self.error = None;
        // A turn that streamed content but never saw its terminal event (e.g.
        // the stream channel closed mid-approval) would be silently discarded
        // here; flush it into history like `record_error` does.
        self.flush_active_turn();
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = self.tr_with(
            TextId::StatusStreamingFrom,
            &[("backend", &self.backend_label)],
        );

        self.history
            .push(HistoryCell::user(shown.unwrap_or_else(|| text.clone())));

        self.start_stream(StreamRequest::User(UserTurn::with_labeled_images(
            text, images,
        )));
    }

    /// Send any prompts queued (steered) while the just-finished turn was
    /// streaming, as one combined follow-up. No-op when nothing is queued.
    ///
    /// Run only from `drain_stream_updates`, after the drain loop — never from
    /// an event handler, see `pending_steering_flush`. The combined user cell is
    /// pushed here rather than at queue time because the finished turn's own
    /// cells have only just landed in `history`; pushing earlier would render
    /// the user's message above output that was still streaming.
    pub(super) fn flush_steering_queue(&mut self) {
        if self.steering_queue.is_empty() || self.is_streaming {
            return;
        }
        // Blank-line join so multiple steered messages read as separate turns
        // to the model rather than one run-on paragraph.
        let combined = std::mem::take(&mut self.steering_queue).join("\n\n");
        self.error = None;
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = self.tr_with(
            TextId::StatusStreamingFrom,
            &[("backend", &self.backend_label)],
        );
        // Added now (not at queue time): the just-finished turn's cells have
        // landed in `history`, so this renders after them, in order.
        self.history.push(HistoryCell::user(combined.clone()));
        // Text only: reaching the queue at all requires passing the
        // `ImageWhileStreaming` refusal in `submit`, so every entry here is a
        // prompt the runtime was steered with.
        self.start_stream(StreamRequest::User(UserTurn::new(combined)));
    }

    pub(super) fn cancel_streaming_turn(&mut self) {
        self.status = self.tr(TextId::StatusCancelling).to_string();
        // Cancel means "changed my mind", so drop the queue here and now rather
        // than waiting for `TurnCancelled` to do it: if the turn had already
        // finished and its `TurnFinished` is still unread, no `TurnCancelled`
        // arrives and the queue would be auto-sent despite the cancel.
        //
        // Unless a cancel is already in flight: everything queued since the
        // first Esc was typed *after* the change of mind — it is the replacement
        // request the composer just confirmed as queued. A second Esc (or a
        // Ctrl+C) while the status still reads "Cancelling…" must not throw it
        // away; `TurnCancelled` landing keeps it for the same reason.
        if !self.cancel_requested {
            self.steering_queue.clear();
            self.pending_steering_flush = false;
        }
        self.cancel_requested = true;
        if let Some(cancel) = &self.stream_cancel {
            cancel.cancel();
        }
    }

    /// Apply queued runtime updates; returns whether anything changed (the
    /// render loop uses this to skip redundant redraws).
    pub fn drain_stream_updates(&mut self) -> bool {
        let Some(mut rx) = self.ui_rx.take() else {
            return false;
        };

        let mut applied = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(update) => {
                    self.apply_ui_update(update);
                    applied = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // The compaction task died without reporting (a panic): nothing will
        // ever arrive on this channel again, and re-installing it below would
        // leave `compacting` set — every prompt and every runtime swap refused
        // until a restart.
        if disconnected && self.compacting() && self.ui_rx.is_none() {
            self.compaction = None;
            self.status = self.tr_with(
                TextId::CompactFailed,
                &[("reason", "the compaction task stopped without an answer")],
            );
            return true;
        }

        // Only put `rx` back when nothing claimed the slot while we were
        // draining. A successor's receiver must never be overwritten: the turn
        // behind it would keep running with nobody observing — tools execute,
        // files change, cost accrues, and an approval request parks forever with
        // no UI to answer it.
        if (self.is_streaming || self.pending_approval.is_some() || self.compacting())
            && self.ui_rx.is_none()
        {
            self.ui_rx = Some(rx);
        }

        // Now that the loop is done (and any trailing `StreamFinished` from the
        // finished turn has been applied), it is safe to start the follow-up.
        if std::mem::take(&mut self.pending_steering_flush) {
            self.flush_steering_queue();
            applied = true;
        }
        applied
    }

    pub(super) fn start_stream(&mut self, request: StreamRequest) {
        self.session_scan = None;
        self.session_switch = None;
        self.cancel_requested = false;
        self.streaming_since = Some(std::time::Instant::now());
        let runtime = Arc::clone(&self.runtime);
        let prompt = match request {
            StreamRequest::User(prompt) => prompt,
            StreamRequest::Approval(approval_id, decision) => {
                let tx = self.ui_tx.as_ref().and_then(|tx| tx.upgrade());
                tokio::spawn(async move {
                    if let Err(error) = runtime.submit_approval(&approval_id, decision).await
                        && let Some(tx) = tx
                    {
                        let _ = tx.send(UiUpdate::ApprovalRejected(error.to_string()));
                    }
                });
                return;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);
        self.ui_tx = Some(tx.downgrade());
        let cancel = CancellationToken::new();
        self.stream_cancel = Some(cancel.clone());
        tokio::spawn(async move {
            let mut turn = runtime.start_turn_cancellable(prompt, &cancel).await;
            let mut cancelling = false;
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancel.cancelled(), if !cancelling => {
                        cancelling = true;
                        // Approval pauses have no running model loop to observe cancellation.
                        turn.cancel().await;
                        continue;
                    }
                    event = turn.recv() => event,
                };
                let Some(event) = event else { break };
                if tx.send(UiUpdate::Event(Box::new(event))).is_err() {
                    return;
                }
            }
            let _ = tx.send(UiUpdate::StreamFinished);
        });
    }

    /// Run `/compact` off the UI thread: the model writes the handover, and
    /// that takes as long as reading the archived history takes. The result
    /// comes back through the same channel a turn's events do; `compaction`
    /// holds the composer and the runtime-swapping commands until it lands,
    /// and Esc or Ctrl+C fires its cancel token meanwhile.
    /// The caller has already checked that nothing else is in flight.
    pub(crate) fn start_compaction(&mut self, focus: Option<&str>) {
        self.session_scan = None;
        self.session_switch = None;
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);
        let running = RunningCompaction::new();
        let cancel = running.cancel.clone();
        self.compaction = Some(running);
        self.status = self.tr(TextId::StatusCompacting).to_string();
        let runtime = Arc::clone(&self.runtime);
        let focus = focus.map(str::to_string);
        tokio::spawn(async move {
            let outcome = runtime.compact_now(focus.as_deref(), &cancel).await;
            let _ = tx.send(UiUpdate::Compacted(Box::new(outcome)));
        });
    }

    /// Esc or Ctrl+C while `/compact` runs: stop it. The answer — cancelled,
    /// history untouched — still arrives through the channel and closes the
    /// busy state, so this only fires the token.
    pub(super) fn cancel_compaction(&mut self) {
        if let Some(running) = &self.compaction {
            running.cancel.cancel();
        }
        self.status = self.tr(TextId::StatusCompactCancelling).to_string();
    }

    pub(super) fn apply_ui_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Event(event) => self.apply_runtime_event(*event),
            UiUpdate::ApprovalRejected(message) => {
                self.status = message;
            }
            UiUpdate::Compacted(outcome) => {
                // Rung for a long one however it ended, except a cancel: the
                // user who pressed Esc is the one person already looking.
                let long = self
                    .compaction
                    .take()
                    .is_some_and(|running| running.since.elapsed() >= NOTIFY_MIN_TURN);
                if long && !matches!(*outcome, deep_code_agent::CompactNowOutcome::Cancelled) {
                    self.request_notification(Notification::CompactionFinished);
                }
                self.finish_compaction(*outcome);
            }
            UiUpdate::StreamFinished => {
                // Reaching here with `is_streaming` still set means the channel
                // closed without any terminal event (runtime panic, or an
                // approval submitted against an already-cancelled turn): every
                // terminal handler clears the flag itself. There is no finished
                // turn for a follow-up to attach to, so drop the queue instead
                // of firing it at whatever unrelated turn comes next.
                if self.is_streaming {
                    self.steering_queue.clear();
                    self.pending_steering_flush = false;
                }
                self.is_streaming = false;
                self.clear_stream_receiver();
            }
        }
    }

    pub(crate) fn record_error(&mut self, message: String) {
        // Keep any streamed partial content visible: flush the active turn
        // into history before appending the error cell, otherwise the next
        // TurnStarted would silently discard it.
        if let Some(active) = &mut self.active_turn {
            active.abandon_unanswered_approvals();
        }
        self.flush_active_turn();
        self.error = Some(message.clone());
        self.status = self.tr(TextId::StatusAgentError).to_string();
        self.history.push(HistoryCell::system(format!(
            "{}{message}",
            self.tr(TextId::ErrorPrefix)
        )));
        self.is_streaming = false;
        self.cancel_requested = false;
        self.pending_approval = None;
        // A failed turn is not a clean hand-off: don't auto-fire queued
        // prompts into a broken state (an API-key error would just re-error
        // each). They stay in the transcript / prompt history to resend.
        self.steering_queue.clear();
        self.pending_steering_flush = false;
        self.clear_stream_receiver();
    }

    pub(crate) fn clear_stream_receiver(&mut self) {
        self.ui_rx = None;
        self.ui_tx = None;
        self.stream_cancel = None;
    }
}
