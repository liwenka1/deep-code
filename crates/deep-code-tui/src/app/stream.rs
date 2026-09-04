//! The streaming-turn lifecycle on [`App`]: submitting a prompt (and the
//! steering queue), the bridge that drains `RuntimeEvent`s into the UI, starting
//! and cancelling a turn, and applying each update. Split from `app/mod.rs` as
//! one cohesive concept; the `impl App` methods here are the whole turn/stream
//! concern.

use super::*;

impl App {
    pub fn submit(&mut self) {
        // An approval is a modal decision — it must be answered, not typed
        // over — so the composer stays inert until it resolves.
        if self.pending_approval.is_some() {
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
        let sent = self.expand_pasted(&display);
        // Never let the API key into the recallable prompt history.
        if !display.starts_with("/apikey") {
            self.remember_prompt(&sent);
        }

        // Slash commands are interactive directives, not conversation — they
        // run immediately (against the live UI) rather than queueing behind a
        // stream, in both idle and streaming states.
        if display.starts_with('/') && self.handle_slash_command(&display) {
            self.clear_input();
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
            if self.steering_queue.len() >= STEERING_QUEUE_CAP {
                // Leave the draft in the composer — losing it is worse than
                // refusing to take more.
                self.status = self.tr_with(
                    TextId::StatusSteeringQueueFull,
                    &[("count", &self.steering_queue.len().to_string())],
                );
                return;
            }
            self.steering_queue.push(sent);
            self.clear_input();
            self.status = self.tr_with(
                TextId::StatusSteeringQueued,
                &[("count", &self.steering_queue.len().to_string())],
            );
            return;
        }

        self.clear_input();
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

        self.history.push(HistoryCell::user(sent.clone()));

        self.start_stream(StreamRequest::User(sent));
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
        self.start_stream(StreamRequest::User(combined));
    }

    pub(super) fn cancel_streaming_turn(&mut self) {
        self.status = self.tr(TextId::StatusCancelling).to_string();
        // Cancel means "changed my mind", so drop the queue here and now rather
        // than waiting for `TurnCancelled` to do it: if the turn had already
        // finished and its `TurnFinished` is still sitting unread in the channel,
        // `cancel_turn` is a no-op on the idle runtime, no `TurnCancelled` ever
        // arrives, and the queue would be auto-sent despite the cancel.
        self.steering_queue.clear();
        self.pending_steering_flush = false;
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

        // Only put `rx` back when nothing claimed the slot while we were
        // draining. A successor's receiver must never be overwritten: the turn
        // behind it would keep running with nobody observing — tools execute,
        // files change, cost accrues, and an approval request parks forever with
        // no UI to answer it.
        if self.is_streaming && self.ui_rx.is_none() {
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
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);
        self.streaming_since = Some(std::time::Instant::now());

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

    pub(super) fn apply_ui_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Event(event) => self.apply_runtime_event(*event),
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
        self.status = self.tr(TextId::StatusAgentError).to_string();
        self.history.push(HistoryCell::system(format!(
            "{}{message}",
            self.tr(TextId::ErrorPrefix)
        )));
        self.is_streaming = false;
        // A failed turn is not a clean hand-off: don't auto-fire queued
        // prompts into a broken state (an API-key error would just re-error
        // each). They stay in the transcript / prompt history to resend.
        self.steering_queue.clear();
        self.clear_stream_receiver();
    }

    pub(crate) fn clear_stream_receiver(&mut self) {
        self.ui_rx = None;
    }
}
