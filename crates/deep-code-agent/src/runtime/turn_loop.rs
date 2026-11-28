use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::compaction::estimate_token_count;
use crate::event::AgentEvent;
use crate::model::{ChatRequest, Usage};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, RuntimeFailureKind, ToolCallId, TurnId, emit};
use crate::runtime::telemetry::probe_prefix;
use crate::runtime::tool_result::{BatchOutcome, tool_call_payload};
use crate::tool::ToolCallAccumulator;

/// Maximum model requests in a single turn. Generous enough that no legitimate
/// agentic turn hits it, low enough that a runaway loop is bounded.
///
/// Raised from 100, which the session record shows was the wrong side of the
/// distribution's tail: across 263 recorded turns the median is 4 requests and
/// p95 is 45, but three turns sat at exactly 100 — the cap, not the model
/// stopping, and each of them in one of the session's largest (most valuable)
/// runs. A bound that fires at all should fire where nothing legitimate lives,
/// and those three observations are censored: how much longer they would have
/// run is unknowable, which is the other reason to move the bound out of the
/// way before drawing conclusions from the distribution.
///
/// Public because it is the documented DEFAULT for `[budget] turn_steps`
/// (`AgentConfig` seeds that field with it), not only a bound. The eval report
/// derives "did this instance hit the cap?" from its round count compared
/// against the *configured* `turn_steps` in force for the run — which is this
/// value only when nothing overrode it — not against the constant directly.
/// The loop itself remains the only thing that enforces the cap.
pub const MAX_TURN_STEPS: u32 = 500;

/// Whether the wrap-up instruction is due: once, at four fifths of whatever
/// budget the caller is counting against.
///
/// Shared by the parent loop (counting model requests) and the sub-agent runner
/// (counting tool calls) so the two cannot drift into "one of them warns and the
/// other is cut off mid-sentence". Four fifths, not "one step before the end":
/// the point is to leave room for the model to FINISH and report, and a single
/// request of warning is not enough room for a turn that still has to write the
/// summary.
pub(crate) fn budget_wrap_up_due(used: u32, limit: u32, already_sent: bool) -> bool {
    !already_sent && limit > 0 && used >= limit.saturating_mul(4) / 5
}

/// Boundary denials tolerated in one turn before the loop stops feeding them
/// back to the model. A denial is deterministic — the kernel/policy refuses
/// the same write however it is spelled — so past this point every further
/// round is spend with no possible payoff; the turn ends with guidance naming
/// the fix only the user can apply (`/add-dir`). Three, not one: a single
/// denial can be incidental (a build script brushing a read-only cache, a
/// probing command) and the first hit already carries the full explanation —
/// the breaker exists for a model that ignores it.
const BOUNDARY_DENIAL_BREAKER: u32 = 3;

impl AgentRuntime {
    /// Spend one step of `turn_id`'s budget: increments the count and hands
    /// back `(steps, wrap_up_sent)`.
    ///
    /// `None` when the state's budget belongs to a newer turn — a superseded
    /// loop still on its way out must not spend, or reset, the allowance of the
    /// turn that replaced it.
    ///
    /// See [`super::state::TurnBudget`] for why this is state and not a local.
    async fn spend_step(&self, turn_id: &TurnId) -> Option<(u32, bool)> {
        let mut state = self.state.lock().await;
        let budget = state.turn_budget.as_mut()?;
        if &budget.turn != turn_id {
            return None;
        }
        budget.steps += 1;
        Some((budget.steps, budget.wrap_up_sent))
    }

    /// Record that the one wrap-up nudge has been sent this turn.
    async fn mark_wrap_up_sent(&self, turn_id: &TurnId) {
        let mut state = self.state.lock().await;
        if let Some(budget) = state.turn_budget.as_mut()
            && &budget.turn == turn_id
        {
            budget.wrap_up_sent = true;
        }
    }

    /// UI language for user-facing runtime text (error diagnostics, approval
    /// previews). Reads the shared [`crate::i18n::SharedLang`] atomic that the
    /// TUI flips on `/lang` via the runtime handle, so a switch is picked up by
    /// the next rendered string without a relaunch or a per-call approval-mode
    /// env re-parse.
    /// The language the runtime renders its user-facing text in. Public so
    /// a front-end can check it follows its own: the sub-agent runner renders
    /// the child's budget notice in the child's language, and a TUI relaunch
    /// hands the new runtime the language the user switched to.
    pub fn ui_lang(&self) -> crate::i18n::Lang {
        self.ui_lang.get()
    }

    /// End the turn when its boundary denials crossed the breaker threshold.
    /// Mirrors the step-limit exit: a user-facing Error event with the remedy
    /// (`/add-dir`, with the denied directory when one was captured).
    /// Returns whether the caller should stop looping. Consulted
    /// after every completed batch — the loop's own and the ones
    /// `handle_approval` resumes after a human answer — so the batch that
    /// crosses the threshold ends the turn, not the one after it.
    pub(super) async fn boundary_breaker_tripped(
        &self,
        turn_id: &crate::runtime::event::TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> bool {
        let denied_path = {
            let state = self.state.lock().await;
            if state.turn_boundary_denials < BOUNDARY_DENIAL_BREAKER {
                return false;
            }
            state.last_boundary_denial_path.clone()
        };
        let message = match denied_path {
            Some(path) => crate::tr_with(
                self.ui_lang(),
                crate::TextId::BoundaryDenialBreakerWithPath,
                &[("path", &path)],
            ),
            None => crate::tr_with(
                self.ui_lang(),
                crate::TextId::BoundaryDenialBreaker,
                &[("limit", &BOUNDARY_DENIAL_BREAKER.to_string())],
            ),
        };
        self.fail_turn(turn_id, tx, RuntimeFailureKind::Execution, message)
            .await;
        true
    }

    /// Drive the model/tool loop until either the turn finishes or an
    /// approval is required. All paths emit a terminal [`RuntimeEvent`]
    /// (`TurnFinished`, `ApprovalRequired`, or `Error`) before returning.
    pub(super) async fn run_loop(
        &self,
        turn_id: TurnId,
        cancel: CancellationToken,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if self.state.lock().await.current_turn_id.as_ref() != Some(&turn_id) {
            return;
        }
        if cancel.is_cancelled() {
            self.finish_turn_cancelled(&turn_id, tx).await;
            return;
        }
        let model = self
            .config
            .resolved_model()
            .expect("begin_turn validates the model");
        let reasoning_effort = self.config.reasoning_effort.as_setting();

        let mut stream_retries = 0u32;

        loop {
            // Bound the model/tool ping-pong. Every iteration is one API request
            // whose context is larger than the last, and the only other exits
            // are cancel, a stream error, an empty tool batch, or an approval
            // park — so a model stuck in a grep/read spiral (or auto-approved by
            // `auto_allow`/Yolo) would bill the user's key until they notice and
            // press Esc. Sub-agents have had a step cap all along; the parent
            // loop, which is the one holding the key, had none.
            //
            // The count and wrap-up guard
            // live in `TurnBudget` (the state) rather than here: an approval
            // park leaves this loop and `resume_after_batch` enters it again, so
            // locals started over at every human answer. `None` means the
            // budget belongs to a newer turn — this loop was superseded and has
            // nothing left to spend.
            let Some((steps, wrap_up_sent)) = self.spend_step(&turn_id).await else {
                return;
            };
            // `0` is the user's explicit "no ceiling": the loop then has no
            // step stop at all, and the only exits left are cancel, a stream
            // error, an empty tool batch or an approval park. That is a real
            // choice with a real cost, which is why it is global-layer only and
            // why the config documents it as removing the backstop.
            let cap = self.config.turn_steps;
            if cap > 0 && steps > cap {
                self.fail_turn(
                    &turn_id,
                    tx,
                    RuntimeFailureKind::StepLimit,
                    crate::tr_with(
                        self.ui_lang(),
                        crate::TextId::TurnStepLimitReached,
                        &[("limit", &cap.to_string())],
                    ),
                )
                .await;
                return;
            }
            if cancel.is_cancelled() {
                self.finish_turn_cancelled(&turn_id, tx).await;
                return;
            }

            // The soft end of the budget, ahead of the hard one: past four
            // fifths of the cap the model is told to converge and hand over,
            // because the hard stop cannot ask for anything — it just ends the
            // turn. Steered so it lands at a tool-batch boundary where appending
            // an entry cannot split a `tool_calls`/`tool` pair — but as a runtime
            // NOTICE, not a user turn: `steer_notice` records a System entry and
            // raises no user-injection event, so it cannot move compaction's
            // task anchor or masquerade as the user's own message.
            //
            // The cost is one prefix break for the rest of the turn, which is
            // the price of saying anything mid-turn at all. Worth it: the
            // alternative is a turn that dies mid-edit with nothing said.
            if budget_wrap_up_due(steps, cap, wrap_up_sent) {
                self.mark_wrap_up_sent(&turn_id).await;
                let text = crate::tr_with(
                    self.ui_lang(),
                    crate::TextId::TurnBudgetWrapUp,
                    &[("used", &steps.to_string()), ("limit", &cap.to_string())],
                );
                let _ = self.steer_notice(text).await;
            }

            // Inside the loop, not once before it. Every iteration appends an
            // assistant message plus its tool results, so a single long agentic
            // turn grew the context without ever re-checking the threshold — the
            // exact shape that overflows the window, and a context-overflow 400
            // is not retriable and has no recovery path.
            self.maybe_compact(model, tx, &cancel, &turn_id).await;

            // Mid-turn steering lands here. The iteration above ran to its
            // `continue`, i.e. a tool batch finished, so the session now ends on
            // `tool` results — appending a `user` entry cannot split a
            // `tool_calls`/`tool` pair, which is exactly why a batch boundary is
            // the only safe point to inject at. Draining *before* the wire
            // messages are derived below is what makes this very request carry
            // the steered prompt; draining *after* `maybe_compact` keeps a
            // message the user just typed out of that pass's archive window (it
            // is the newest thing in the transcript, not history).
            self.drain_steering(&turn_id, tx).await;

            // The transcript and the previous prefix are read under one lock:
            // this is the only place a request's exact messages exist next to
            // what the last turn sent, and the last iteration's probe is the one
            // telemetry reports.
            let (projection, prior_prefix, reply_anchor) = {
                let state = self.state.lock().await;
                (
                    state
                        .session
                        .wire_messages_with_image_occurrences(Some(&turn_id)),
                    state.last_prefix,
                    state.session.entries().last().cloned(),
                )
            };
            let request = ChatRequest::streaming(model, projection.messages)
                .with_tools(self.tools.chat_tools())
                .with_reasoning_effort(reasoning_effort);

            let opened = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                opened = self.open_turn_stream(request, &projection.submissions, &cancel) => Some(opened),
            };

            let mut stream = match opened {
                None => {
                    self.finish_turn_cancelled(&turn_id, tx).await;
                    return;
                }
                Some(Ok(stream)) => stream,
                Some(Err(error)) => {
                    self.fail_turn(
                        &turn_id,
                        tx,
                        RuntimeFailureKind::Execution,
                        error.user_message(self.ui_lang()),
                    )
                    .await;
                    return;
                }
            };

            let prefix = probe_prefix(&stream.prepared_request().messages, prior_prefix);
            let estimated_context_tokens =
                estimate_token_count(&stream.prepared_request().messages);
            let image_deliveries: Vec<_> = stream
                .image_deliveries()
                .iter()
                .map(|image| {
                    let mut image = image.clone();
                    image.observation_id = projection
                        .image_occurrences
                        .get(&(image.message_index, image.block_index))
                        .cloned();
                    image
                })
                .collect();
            self.record_image_projection(&turn_id, model, &image_deliveries, tx)
                .await;

            let mut accumulator = ToolCallAccumulator::default();
            let mut tool_call_ids: HashMap<u32, ToolCallId> = HashMap::new();
            let mut text_buffer = String::new();
            let mut reasoning_buffer = String::new();
            let mut last_usage: Option<Usage> = None;
            let mut failure = None;
            let mut pending = projection.has_pending;

            loop {
                let event = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    event = stream.next() => match event {
                        Some(event) => event,
                        None => break,
                    },
                };
                if pending
                    && event
                        .as_ref()
                        .is_ok_and(|event| !matches!(event, AgentEvent::Error { .. }))
                {
                    pending = false;
                    self.confirm_submissions(&turn_id).await;
                }
                match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        text_buffer.push_str(&text);
                        emit(
                            tx,
                            RuntimeEvent::AssistantDelta {
                                turn_id: turn_id.clone(),
                                text,
                            },
                        );
                    }
                    Ok(AgentEvent::ReasoningDelta { text }) => {
                        reasoning_buffer.push_str(&text);
                        emit(
                            tx,
                            RuntimeEvent::ReasoningDelta {
                                turn_id: turn_id.clone(),
                                text,
                            },
                        );
                    }
                    Ok(AgentEvent::ToolCallDelta { delta }) => {
                        let index = delta.index.unwrap_or(0);
                        let tool_call_id = if let Some(id) = delta.id.clone() {
                            let tool_call_id = ToolCallId::from(id);
                            tool_call_ids.insert(index, tool_call_id.clone());
                            Some(tool_call_id)
                        } else {
                            tool_call_ids.get(&index).cloned()
                        };
                        let arguments_delta = delta
                            .function
                            .as_ref()
                            .and_then(|function| function.arguments.clone());
                        if let Some(tool_call_id) = tool_call_id {
                            emit(
                                tx,
                                RuntimeEvent::ToolCallUpdated {
                                    turn_id: turn_id.clone(),
                                    tool_call_id,
                                    arguments_delta,
                                },
                            );
                        }
                        accumulator.push_delta(delta);
                    }
                    Ok(AgentEvent::Done { usage }) => {
                        // Price each request as it completes: a multi-tool
                        // turn makes several, and summing here keeps turn and
                        // session costs honest even when the turn is later
                        // cancelled or errors out mid-way.
                        if let Some(usage) = usage.as_ref() {
                            self.accumulate_request_usage(model, usage).await;
                        }
                        last_usage = usage;
                    }
                    Ok(AgentEvent::Error { message }) => {
                        failure = Some(message);
                        break;
                    }
                    Err(error) => {
                        failure = Some(error.user_message(self.ui_lang()));
                        break;
                    }
                }
            }

            stream_retries += stream.retries_used();

            if cancel.is_cancelled() {
                // Partial assistant output stays in the transcript; partial
                // tool-call deltas are discarded before they become real
                // calls, so no tool_call/tool pairing is broken.
                self.record_partial_reply(
                    &turn_id,
                    reply_anchor.as_ref(),
                    text_buffer,
                    reasoning_buffer,
                )
                .await;
                self.finish_turn_cancelled(&turn_id, tx).await;
                return;
            }

            if let Some(message) = failure {
                // Same semantics as cancellation: streamed partial output is
                // kept (no tool_calls were pushed, so pairing is intact).
                self.record_partial_reply(
                    &turn_id,
                    reply_anchor.as_ref(),
                    text_buffer,
                    reasoning_buffer,
                )
                .await;
                self.fail_turn(&turn_id, tx, RuntimeFailureKind::Execution, message)
                    .await;
                return;
            }

            let calls = match accumulator.finish() {
                Ok(calls) => calls,
                Err(error) => {
                    self.record_partial_reply(
                        &turn_id,
                        reply_anchor.as_ref(),
                        text_buffer,
                        reasoning_buffer,
                    )
                    .await;
                    self.fail_turn(
                        &turn_id,
                        tx,
                        RuntimeFailureKind::Execution,
                        error.to_string(),
                    )
                    .await;
                    return;
                }
            };

            {
                let mut state = self.state.lock().await;
                // A successor can start while this loop waits for the lock,
                // including after a successful stream. Never append an old
                // batch to the successor's history or execute its calls.
                if state.current_turn_id.as_ref() != Some(&turn_id) || cancel.is_cancelled() {
                    drop(state);
                    self.record_partial_reply(
                        &turn_id,
                        reply_anchor.as_ref(),
                        text_buffer,
                        reasoning_buffer,
                    )
                    .await;
                    self.finish_turn_cancelled(&turn_id, tx).await;
                    return;
                }
                state.session.push_assistant(
                    turn_id.clone(),
                    text_buffer,
                    reasoning_buffer,
                    calls.iter().map(tool_call_payload).collect(),
                );
            }

            if calls.is_empty() {
                // Keep new turns out until the final reply and its workspace
                // checkpoint describe the same boundary.
                let turn_start = self.turn_start.lock().await;
                if self.live_turn_id().await.as_ref() != Some(&turn_id) || cancel.is_cancelled() {
                    drop(turn_start);
                    self.finish_turn_cancelled(&turn_id, tx).await;
                    return;
                }
                let usage = last_usage.clone();
                let telemetry = self
                    .build_turn_telemetry(
                        model,
                        usage.as_ref(),
                        prefix,
                        estimated_context_tokens,
                        stream_retries,
                    )
                    .await;
                self.snapshot_completed_turn(&turn_id, &cancel, tx).await;
                self.finish_turn(&turn_id).await;
                drop(turn_start);
                // Publish the completed save result, not the previous batch's
                // error (which may still be empty while this write fails).
                if let Some(persistence) = &self.persistence {
                    persistence.actor.flush().await;
                }
                self.emit_session_updated(tx).await;
                // Surface any LSP warnings buffered since the last edit tool
                // before the turn closes, so none are stranded to the next one.
                self.drain_lsp_warnings(tx).await;
                emit(
                    tx,
                    RuntimeEvent::TurnFinished {
                        turn_id: turn_id.clone(),
                        usage: last_usage,
                        telemetry: Some(telemetry),
                    },
                );
                return;
            }

            for call in &calls {
                emit(
                    tx,
                    RuntimeEvent::ToolCallStarted {
                        turn_id: turn_id.clone(),
                        tool_call_id: ToolCallId::from(call.id.clone()),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                );
            }

            self.persist().await;
            self.emit_session_updated(tx).await;

            match self
                .process_tool_batch(VecDeque::from(calls), &turn_id, &cancel, tx)
                .await
            {
                // Loop again: feed tool results back into the next chat turn —
                // unless the batch pushed the turn's boundary denials past the
                // breaker, in which case another model round cannot help (the
                // fence is deterministic; only the user can move it) and the
                // turn ends with guidance instead of more spend.
                BatchOutcome::Completed => {
                    if self.boundary_breaker_tripped(&turn_id, tx).await {
                        return;
                    }
                    continue;
                }
                BatchOutcome::AwaitingApproval | BatchOutcome::Cancelled => return,
            }
        }
    }

    async fn record_partial_reply(
        &self,
        turn_id: &TurnId,
        anchor: Option<&std::sync::Arc<crate::session_entry::SessionEntry>>,
        text: String,
        reasoning: String,
    ) {
        if text.is_empty() && reasoning.is_empty() {
            return;
        }
        let Some(anchor) = anchor else { return };
        let inserted = self
            .state
            .lock()
            .await
            .session
            .insert_partial_assistant_after(anchor, turn_id.clone(), text, reasoning);
        // A superseded turn's finish_turn is a no-op, so save this insertion
        // here as well as on the ordinary cancellation/error path.
        if inserted {
            self.persist().await;
        }
    }

    async fn record_image_projection(
        &self,
        turn_id: &TurnId,
        model: &str,
        images: &[crate::image::ImageDelivery],
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if images.is_empty() {
            return;
        }
        let changed = {
            let mut state = self.state.lock().await;
            if state.current_turn_id.as_ref() != Some(turn_id) {
                return;
            }
            if state.current_turn.is_none() {
                return;
            }
            let changes = state.image_projections.changes(model, images);
            if !changes.is_empty() {
                state
                    .current_turn
                    .as_mut()
                    .expect("turn checked above")
                    .image_requests
                    .push(crate::session_store::ImageRequestRecord {
                        model: model.to_owned(),
                        images: changes.clone(),
                    });
                let mut budget = crate::session_store::IMAGE_REPORT_BUDGET_BYTES;
                state
                    .current_turn
                    .as_mut()
                    .expect("turn checked above")
                    .limit_image_reports(&mut budget);
            }
            changes
        };
        emit(
            tx,
            RuntimeEvent::ImageRequestPrepared {
                turn_id: turn_id.clone(),
                model: model.to_owned(),
                images: images.to_vec(),
            },
        );
        if !changed.is_empty() {
            let omitted: Vec<_> = changed.iter().filter(|image| !image.included).collect();
            if !omitted.is_empty() {
                let reasons = omitted
                    .iter()
                    .take(4)
                    .map(|image| {
                        format!(
                            "{}: {}",
                            image.image,
                            image.reason.as_deref().unwrap_or("unavailable")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                emit(
                    tx,
                    RuntimeEvent::Warning {
                        message: format!(
                            "图片观察未包含在本次模型请求中 / image observations unavailable to this request ({model}): {reasons}"
                        ),
                    },
                );
            }
            self.persist().await;
        }
    }

    /// Record any steered prompts in the session and tell the UI they landed.
    ///
    /// One lock scope for the whole batch: a `steer()` racing this drain lands
    /// wholly in this batch or wholly in the next, never half in each. Persist
    /// and the announcements happen after the lock is released, and the events
    /// go out in arrival order — the UI pops its own pending list FIFO, so that
    /// order is part of the contract.
    async fn drain_steering(&self, turn_id: &TurnId, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        use super::Steered;
        let (events, changed) = {
            let mut state = self.state.lock().await;
            if state.current_turn_id.as_ref() != Some(turn_id) {
                return;
            }
            let mut events = Vec::new();
            let mut changed = false;
            for steered in std::mem::take(&mut state.steering) {
                match steered {
                    Steered::User {
                        prompt,
                        content,
                        unsent,
                    } => {
                        if state.can_retry(prompt.retry_drafts()) {
                            state
                                .session
                                .push_pending_user(turn_id.clone(), content, unsent);
                            changed = true;
                            events.push(RuntimeEvent::UserMessageInjected {
                                turn_id: turn_id.clone(),
                                text: prompt.text,
                            });
                        } else {
                            events.push(RuntimeEvent::UserRetryUnavailable {
                                turn_id: turn_id.clone(),
                                draft_ids: prompt.retry_of,
                            });
                        }
                    }
                    // A runtime nudge (the budget wrap-up): recorded as a System
                    // entry, never a user one. As a user entry it would become
                    // the newest user message and pull compaction's anchor off
                    // the real task — archiving the task into a summary line
                    // that is then persisted. System entries are not the anchor,
                    // so the task survives (and a mid-stream System notice is
                    // itself archivable once the turn moves on).
                    Steered::Notice(text) => {
                        state.session.push_system(text);
                        changed = true;
                    }
                }
            }
            (events, changed)
        };
        if changed {
            self.persist().await;
        }
        for event in events {
            emit(tx, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::budget_wrap_up_due;

    /// The wrap-up fires once, at four fifths of whatever budget is in force,
    /// and never when there is no budget. Pinned as a truth table because the
    /// shipped cap (500) is far above any reachable turn, so the threshold
    /// cannot be exercised end-to-end at its real value.
    #[test]
    fn wrap_up_is_due_once_at_four_fifths() {
        assert!(!budget_wrap_up_due(0, 500, false));
        assert!(!budget_wrap_up_due(399, 500, false));
        assert!(budget_wrap_up_due(400, 500, false));
        assert!(budget_wrap_up_due(500, 500, false));
        assert!(
            !budget_wrap_up_due(400, 500, true),
            "asked once per turn, not on every step past the threshold"
        );
        assert!(
            !budget_wrap_up_due(10, 0, false),
            "an unlimited budget has no four fifths"
        );
        // The sub-agent counts tool calls instead of requests; same fraction.
        assert!(budget_wrap_up_due(160, 200, false));
        assert!(!budget_wrap_up_due(159, 200, false));
    }
}
