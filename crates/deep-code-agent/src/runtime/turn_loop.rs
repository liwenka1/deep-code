use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;

use crate::model_route::{RouteContext, resolve_turn_route};
use crate::client::LlmClient;
use crate::compaction::{estimate_token_count, stable_prefix_fingerprint};
use crate::event::AgentEvent;
use crate::model::{ChatRequest, Usage};
use crate::model_registry::{DEEPSEEK_V4_PRO, context_window_for_model};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, emit};
use crate::runtime::tool_result::{BatchOutcome, runtime_error_from_tool_error, tool_call_payload};
use crate::tool::ToolCallAccumulator;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// UI language for user-facing runtime text (error diagnostics, approval
    /// previews). Reads the shared [`crate::i18n::SharedLang`] atomic that the
    /// TUI flips on `/lang` via the runtime handle, so a switch is picked up by
    /// the next rendered string without a relaunch or a per-call env re-parse.
    pub(super) fn ui_lang(&self) -> crate::i18n::Lang {
        self.ui_lang.get()
    }

    /// Drive the model/tool loop until either the turn finishes or an
    /// approval is required. All paths emit a terminal [`RuntimeEvent`]
    /// (`TurnFinished`, `ApprovalRequired`, or `Error`) before returning.
    pub(super) async fn run_loop(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let (user_prompt, cancel, route_ctx) = {
            let state = self.state.lock().await;
            let context_tokens = estimate_token_count(&state.session.wire_messages());
            (
                state.current_prompt.clone().unwrap_or_default(),
                state.cancel.clone(),
                RouteContext {
                    context_tokens,
                    context_window: context_window_for_model(DEEPSEEK_V4_PRO),
                    escalated: state.cascade_escalated,
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

        if self.maybe_compact(&route.effective_model, tx).await {
            // compaction event already emitted; continue with trimmed history
        }

        let mut stream_retries = 0u32;

        loop {
            if cancel.is_cancelled() {
                self.finish_turn_cancelled(&turn_id, tx).await;
                return;
            }
            let (messages, prefix_hash) = {
                let state = self.state.lock().await;
                let messages = state.session.wire_messages();
                let prefix_hash = stable_prefix_fingerprint(&messages);
                (messages, prefix_hash)
            };

            let estimated_context_tokens = estimate_token_count(&messages);

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
            let mut stream = match opened {
                None => {
                    self.finish_turn_cancelled(&turn_id, tx).await;
                    return;
                }
                Some(Ok(stream)) => stream,
                Some(Err(error)) => {
                    emit(
                        tx,
                        RuntimeEvent::Error {
                            turn_id: Some(turn_id.clone()),
                            message: error.user_message(self.ui_lang()),
                        },
                    );
                    self.abort_turn().await;
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
                self.abort_turn().await;
                return;
            }

            let calls = match accumulator.finish() {
                Ok(calls) => calls,
                Err(error) => {
                    emit(
                        tx,
                        runtime_error_from_tool_error(error, Some(turn_id.clone())),
                    );
                    self.abort_turn().await;
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
                self.snapshot_turn("after_turn", tx).await;
                let usage = last_usage.clone();
                let telemetry = self
                    .build_turn_telemetry(
                        &route,
                        usage.as_ref(),
                        prefix_hash,
                        estimated_context_tokens,
                        stream_retries,
                    )
                    .await;
                self.finish_turn(usage.clone()).await;
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
                // Loop again: feed tool results back into the next chat turn.
                BatchOutcome::Completed => continue,
                BatchOutcome::AwaitingApproval | BatchOutcome::Cancelled => return,
            }
        }
    }
}
