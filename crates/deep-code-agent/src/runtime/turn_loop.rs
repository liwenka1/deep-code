use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;

use crate::compaction::estimate_token_count;
use crate::error::AgentError;
use crate::event::AgentEvent;
use crate::model::{ChatRequest, Usage};
use crate::model_registry::{DEEPSEEK_V4_PRO, context_window_for_model};
use crate::model_route::{RouteContext, resolve_turn_route};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::telemetry::probe_prefix;
use crate::runtime::tool_result::{BatchOutcome, runtime_error_from_tool_error, tool_call_payload};
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
/// Public because it is a measurement key and not only a bound: the eval
/// report derives "did this instance hit the cap?" from its own round count
/// compared against this value, and a private copy there would drift the
/// moment this number moves. Exported for that comparison — the loop itself
/// remains the only thing that enforces it.
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
    /// UI language for user-facing runtime text (error diagnostics, approval
    /// previews). Reads the shared [`crate::i18n::SharedLang`] atomic that the
    /// TUI flips on `/lang` via the runtime handle, so a switch is picked up by
    /// the next rendered string without a relaunch or a per-call approval-mode
    /// env re-parse.
    /// Crate-wide rather than `pub(super)`: the sub-agent runner renders the
    /// child's own budget notice and must use the child's language, and the
    /// child runtime is the thing that knows it.
    pub(crate) fn ui_lang(&self) -> crate::i18n::Lang {
        self.ui_lang.get()
    }

    /// End the turn when its boundary denials crossed the breaker threshold.
    /// Mirrors the step-limit exit: a user-facing Error event with the remedy
    /// (`/add-dir`, with the denied directory when one was captured), then
    /// `abort_turn`. Returns whether the caller should stop looping. Consulted
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
        emit(
            tx,
            RuntimeEvent::Error {
                turn_id: Some(turn_id.clone()),
                message,
            },
        );
        self.abort_turn(turn_id).await;
        true
    }

    /// Drive the model/tool loop until either the turn finishes or an
    /// approval is required. All paths emit a terminal [`RuntimeEvent`]
    /// (`TurnFinished`, `ApprovalRequired`, or `Error`) before returning.
    pub(super) async fn run_loop(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let (user_prompt, cancel, route_ctx) = {
            let state = self.state.lock().await;
            let context_tokens = estimate_token_count(&state.session.wire_messages());
            let prompt = state.current_prompt.clone().unwrap_or_default();
            (
                prompt.text.clone(),
                state.cancel.clone(),
                RouteContext {
                    context_tokens,
                    context_window: context_window_for_model(DEEPSEEK_V4_PRO),
                    escalated: state.cascade_escalated,
                    has_images: prompt.has_images(),
                },
            )
        };
        let turn_id = self.current_turn_id().await;
        if cancel.is_cancelled() {
            self.finish_turn_cancelled(&turn_id, tx).await;
            return;
        }
        // Routing is deterministic and local (no network): Flash-first unless a
        // hard rule or difficulty keyword forces Pro, plus the cascade latch.
        let mut route = resolve_turn_route(
            &self.config,
            &self.registry,
            &user_prompt,
            self.is_subagent,
            route_ctx,
            self.ui_lang(),
        );

        let mut stream_retries = 0u32;
        let mut steps = 0u32;
        // One forced-compaction rescue per turn at most (see the `Some(Err)`
        // arm below). A second overflow means the oversized part is this turn's
        // own retained tail, which compaction structurally cannot reach —
        // retrying again would only re-bill the same rejected request.
        let mut overflow_rescued = false;
        // One wrap-up instruction per turn (see `budget_wrap_up_due`).
        let mut budget_notice_sent = false;

        loop {
            // Bound the model/tool ping-pong. Every iteration is one API request
            // whose context is larger than the last, and the only other exits
            // are cancel, a stream error, an empty tool batch, or an approval
            // park — so a model stuck in a grep/read spiral (or auto-approved by
            // `auto_allow`/Yolo) would bill the user's key until they notice and
            // press Esc. Sub-agents have had a step cap all along; the parent
            // loop, which is the one holding the key, had none.
            steps += 1;
            // `0` is the user's explicit "no ceiling": the loop then has no
            // step stop at all, and the only exits left are cancel, a stream
            // error, an empty tool batch or an approval park. That is a real
            // choice with a real cost, which is why it is global-layer only and
            // why the config documents it as removing the backstop.
            let cap = self.config.turn_steps;
            if cap > 0 && steps > cap {
                emit(
                    tx,
                    RuntimeEvent::Error {
                        turn_id: Some(turn_id.clone()),
                        message: crate::tr_with(
                            self.ui_lang(),
                            crate::TextId::TurnStepLimitReached,
                            &[("limit", &cap.to_string())],
                        ),
                    },
                );
                self.abort_turn(&turn_id).await;
                return;
            }
            if cancel.is_cancelled() {
                self.finish_turn_cancelled(&turn_id, tx).await;
                return;
            }

            // The soft end of the budget, ahead of the hard one: past four
            // fifths of the cap the model is told to converge and hand over,
            // because the hard stop cannot ask for anything — it just ends the
            // turn. Steered rather than pushed into the transcript directly, so
            // it lands at a tool-batch boundary where appending a `user` entry
            // cannot split a `tool_calls`/`tool` pair.
            //
            // The cost is one prefix break for the rest of the turn, which is
            // the price of saying anything mid-turn at all. Worth it: the
            // alternative is a turn that dies mid-edit with nothing said.
            if budget_wrap_up_due(steps, cap, budget_notice_sent) {
                budget_notice_sent = true;
                let text = crate::tr_with(
                    self.ui_lang(),
                    crate::TextId::TurnBudgetWrapUp,
                    &[("used", &steps.to_string()), ("limit", &cap.to_string())],
                );
                let _ = self.steer(text).await;
            }

            // Inside the loop, not once before it. Every iteration appends an
            // assistant message plus its tool results, so a single long agentic
            // turn grew the context without ever re-checking the threshold — the
            // exact shape that overflows the window, and a context-overflow 400
            // is not retriable and has no recovery path.
            if self.maybe_compact(&route.effective_model, tx, false).await {
                // compaction event already emitted; continue with trimmed history
            }

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
            let (messages, prior_prefix) = {
                let state = self.state.lock().await;
                (state.session.wire_messages(), state.last_prefix)
            };
            // Resolving an image reads a file, so it happens outside the lock.
            //
            // Capability-aware, and that is what makes the session usable after a
            // model switch: a transcript that already contains an image must stay
            // sendable on a model without vision, so `hydrate` replaces those
            // images with a note instead of the request being refused. Refusing
            // here would wedge the session for good — the image is in the history,
            // every later turn re-derives it, so *every* turn including the
            // text-only ones would be refused, with advice ("remove the images")
            // the user cannot act on because the images left the draft at submit.
            // An id the registry does not know is treated as accepting (`map_or`),
            // so a self-hosted or newer model is the server's call rather than
            // ours.
            let accepts_images = self
                .registry
                .info_for(&route.effective_model)
                .is_none_or(|entry| entry.supports_vision);
            let messages = crate::image::hydrate_for(
                messages,
                Some(self.config.vision_detail),
                accepts_images,
                crate::image::MAX_TOTAL_BYTES,
            );

            let prefix = probe_prefix(&messages, prior_prefix);

            let estimated_context_tokens = estimate_token_count(&messages);
            // Our own read on how close this request is to the window, used only
            // for the overflow rescue below. The provider's error wording is not
            // ours to rely on across versions; this number is, so the rescue has
            // a signal that does not depend on it.
            let near_window = estimated_context_tokens
                >= context_window_for_model(&route.effective_model).saturating_mul(95) / 100;

            let mut request = ChatRequest::streaming(route.effective_model.clone(), messages)
                .with_tools(self.tools.chat_tools());
            if let Some(effort) = route.effective_effort.as_api_value() {
                request = request.with_reasoning_effort(effort);
            }

            let opened = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                opened = self.open_turn_stream(&mut route, request) => Some(opened),
            };
            // A 400 the provider blames on the context window, OR any 400 at a
            // context we ourselves read as within 5% of it. Two independent
            // signals on purpose: the phrase match is the provider's wording,
            // `near_window` is our own estimate and survives a rewording.
            let overflow_refusal = |error: &AgentError| {
                error.blames_context_window()
                    || (near_window
                        && matches!(error, AgentError::Api { status, .. } if status.as_u16() == 400))
            };

            let mut stream = match opened {
                None => {
                    self.finish_turn_cancelled(&turn_id, tx).await;
                    return;
                }
                Some(Ok(stream)) => stream,
                Some(Err(error)) if !overflow_rescued && overflow_refusal(&error) => {
                    // The provider refused the request for being over the model's
                    // window. Shrinking history is the only recovery there is,
                    // and it has to be FORCED: the estimate that normally gates
                    // compaction is the very number that just proved too
                    // optimistic.
                    //
                    // The loop simply continues — the retry is the same request
                    // over a now-shorter history, so a UI must not read it as a
                    // new turn. Hence a warning rather than an error, and no
                    // TurnStarted.
                    overflow_rescued = true;
                    if self.compact_session(Some(tx)).await.is_none() {
                        // Nothing left to archive: the oversized part is this
                        // turn's own retained tail — a single huge tool result,
                        // say — which the retention rule keeps by design and
                        // compaction can never reach. Report that instead of
                        // re-raising the raw 400, and name the way out.
                        emit(
                            tx,
                            RuntimeEvent::Error {
                                turn_id: Some(turn_id.clone()),
                                message: crate::tr_with(
                                    self.ui_lang(),
                                    crate::TextId::ErrContextOverflow,
                                    &[("estimate", &estimated_context_tokens.to_string())],
                                ),
                            },
                        );
                        self.abort_turn(&turn_id).await;
                        return;
                    }
                    emit(
                        tx,
                        RuntimeEvent::Warning {
                            message: crate::tr(
                                self.ui_lang(),
                                crate::TextId::ContextOverflowRescued,
                            )
                            .to_string(),
                        },
                    );
                    continue;
                }
                Some(Err(error)) => {
                    emit(
                        tx,
                        RuntimeEvent::Error {
                            turn_id: Some(turn_id.clone()),
                            message: error.user_message(self.ui_lang()),
                        },
                    );
                    self.abort_turn(&turn_id).await;
                    return;
                }
            };

            let mut accumulator = ToolCallAccumulator::default();
            let mut tool_call_ids: HashMap<u32, ToolCallId> = HashMap::new();
            let mut text_buffer = String::new();
            let mut reasoning_buffer = String::new();
            let mut last_usage: Option<Usage> = None;
            let mut had_error = false;
            let mut cancelled = false;

            loop {
                let event = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    event = stream.next() => match event {
                        Some(event) => event,
                        None => break,
                    },
                };
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
                            self.accumulate_request_usage(&route.effective_model, usage)
                                .await;
                        }
                        last_usage = usage;
                    }
                    Ok(AgentEvent::Error { message }) => {
                        emit(
                            tx,
                            RuntimeEvent::Error {
                                turn_id: Some(turn_id.clone()),
                                message,
                            },
                        );
                        had_error = true;
                        // `Error` is terminal for the turn (consumers stop
                        // observing on it), so stop reading here: a provider
                        // that follows one error frame with another must not
                        // produce a second terminal event. The transport-error
                        // arm below gets the same exit from `GuardedStream`,
                        // which fuses itself after an `Err`.
                        break;
                    }
                    Err(error) => {
                        emit(
                            tx,
                            RuntimeEvent::Error {
                                turn_id: Some(turn_id.clone()),
                                message: error.user_message(self.ui_lang()),
                            },
                        );
                        had_error = true;
                    }
                }
            }

            stream_retries += stream.retries_used();

            if cancelled {
                // Partial assistant output stays in the transcript; partial
                // tool-call deltas are discarded before they become real
                // calls, so no tool_call/tool pairing is broken.
                if !text_buffer.is_empty() || !reasoning_buffer.is_empty() {
                    let mut state = self.state.lock().await;
                    state
                        .session
                        .push_assistant(text_buffer, reasoning_buffer, Vec::new());
                }
                self.finish_turn_cancelled(&turn_id, tx).await;
                return;
            }

            if had_error {
                // Same semantics as cancellation: streamed partial output is
                // kept (no tool_calls were pushed, so pairing is intact).
                if !text_buffer.is_empty() || !reasoning_buffer.is_empty() {
                    let mut state = self.state.lock().await;
                    state
                        .session
                        .push_assistant(text_buffer, reasoning_buffer, Vec::new());
                }
                self.abort_turn(&turn_id).await;
                return;
            }

            let calls = match accumulator.finish() {
                Ok(calls) => calls,
                Err(error) => {
                    emit(
                        tx,
                        runtime_error_from_tool_error(error, Some(turn_id.clone())),
                    );
                    self.abort_turn(&turn_id).await;
                    return;
                }
            };

            if calls.is_empty() {
                let mut state = self.state.lock().await;
                state
                    .session
                    .push_assistant(text_buffer, reasoning_buffer, Vec::new());
                drop(state);
                self.persist().await;
                self.emit_session_updated(tx).await;
                // No end-of-turn snapshot: it would be near-identical to the
                // next turn's before_turn (only edits the user makes between
                // turns differ), and rewind/restore key off before_turn — so
                // one snapshot per turn buys the same protection at half the
                // copy cost and twice the retained history.
                let usage = last_usage.clone();
                let telemetry = self
                    .build_turn_telemetry(
                        &route,
                        usage.as_ref(),
                        prefix,
                        estimated_context_tokens,
                        stream_retries,
                    )
                    .await;
                self.finish_turn(&turn_id).await;
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

            let payloads = calls.iter().map(tool_call_payload).collect::<Vec<_>>();
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

            {
                let mut state = self.state.lock().await;
                state
                    .session
                    .push_assistant(text_buffer, reasoning_buffer, payloads);
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

    /// Record any steered prompts in the session and tell the UI they landed.
    ///
    /// One lock scope for the whole batch: a `steer()` racing this drain lands
    /// wholly in this batch or wholly in the next, never half in each. Persist
    /// and the announcements happen after the lock is released, and the events
    /// go out in arrival order — the UI pops its own pending list FIFO, so that
    /// order is part of the contract.
    async fn drain_steering(&self, turn_id: &TurnId, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let drained: Vec<crate::runtime::UserTurn> = {
            let mut state = self.state.lock().await;
            let drained: Vec<crate::runtime::UserTurn> = std::mem::take(&mut state.steering).into();
            for prompt in &drained {
                state
                    .session
                    .push_user_with_images(&prompt.text, prompt.images.clone());
            }
            drained
        };
        if drained.is_empty() {
            return;
        }
        self.persist().await;
        for prompt in drained {
            emit(
                tx,
                RuntimeEvent::UserMessageInjected {
                    turn_id: turn_id.clone(),
                    text: prompt.text,
                },
            );
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
