use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::auto_mode::resolve_turn_route;
use crate::client::LlmClient;
use crate::compaction::{estimate_token_count, stable_prefix_fingerprint};
use crate::event::AgentEvent;
use crate::message::Message;
use crate::model::{ChatRequest, Usage};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, emit};
use crate::runtime::state::PendingToolCall;
use crate::runtime::tool_result::{runtime_error_from_tool_error, tool_call_payload};
use crate::tool::{ToolCallAccumulator, ToolRunOutcome};

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Drive the model/tool loop until either the turn finishes or an
    /// approval is required. All paths emit a terminal [`RuntimeEvent`]
    /// (`TurnFinished`, `ApprovalRequired`, or `Error`) before returning.
    pub(super) async fn run_loop(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let user_prompt = {
            let state = self.state.lock().await;
            state.current_prompt.clone().unwrap_or_default()
        };
        let turn_id = self.current_turn_id().await;
        let mut route =
            resolve_turn_route(&self.config, &self.registry, &user_prompt, self.is_subagent);

        if self.maybe_compact(&route.effective_model, tx).await {
            // compaction event already emitted; continue with trimmed history
        }

        loop {
            let (messages, prefix_hash) = {
                let state = self.state.lock().await;
                let messages = state.session.messages().to_vec();
                let prefix_hash = stable_prefix_fingerprint(&messages);
                (messages, prefix_hash)
            };

            let estimated_context_tokens = estimate_token_count(&messages);

            let mut request = ChatRequest::streaming(route.effective_model.clone(), messages)
                .with_tools(self.tools.chat_tools());
            if let Some(effort) = route.effective_effort.as_api_value() {
                request = request.with_reasoning_effort(effort);
            }

            let mut stream = match self.stream_with_fallback(&mut route, request).await {
                Ok(stream) => stream,
                Err(error) => {
                    emit(
                        tx,
                        RuntimeEvent::Error {
                            turn_id: Some(turn_id.clone()),
                            message: error.user_message(),
                        },
                    );
                    self.abort_turn().await;
                    return;
                }
            };

            let mut accumulator = ToolCallAccumulator::default();
            let mut tool_call_ids: HashMap<u32, ToolCallId> = HashMap::new();
            let mut text_buffer = String::new();
            let mut last_usage: Option<Usage> = None;
            let mut had_error = false;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        text_buffer.push_str(&text);
                        emit(
                            tx,
                            RuntimeEvent::AssistantDelta {
                                turn_id: turn_id.clone(),
                                text: text.clone(),
                            },
                        );
                        emit(tx, RuntimeEvent::Provider(AgentEvent::TextDelta { text }));
                    }
                    Ok(AgentEvent::ReasoningDelta { text }) => {
                        emit(
                            tx,
                            RuntimeEvent::ReasoningDelta {
                                turn_id: turn_id.clone(),
                                text: text.clone(),
                            },
                        );
                        emit(
                            tx,
                            RuntimeEvent::Provider(AgentEvent::ReasoningDelta { text }),
                        );
                    }
                    Ok(AgentEvent::ToolCallDelta { delta }) => {
                        let forwarded = delta.clone();
                        let index = forwarded.index.unwrap_or(0);
                        let tool_call_id = if let Some(id) = forwarded.id.clone() {
                            let tool_call_id = ToolCallId::from(id);
                            tool_call_ids.insert(index, tool_call_id.clone());
                            Some(tool_call_id)
                        } else {
                            tool_call_ids.get(&index).cloned()
                        };
                        let arguments_delta = forwarded
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
                        emit(
                            tx,
                            RuntimeEvent::Provider(AgentEvent::ToolCallDelta { delta: forwarded }),
                        );
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
                                message: error.user_message(),
                            },
                        );
                        had_error = true;
                    }
                }
            }

            if had_error {
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
                state.session.push(Message::assistant(text_buffer));
                drop(state);
                self.persist().await;
                self.emit_session_updated(tx).await;
                self.snapshot_turn("after_turn", tx);
                let usage = last_usage.clone();
                let telemetry = self.build_turn_telemetry(
                    &route,
                    usage.as_ref(),
                    prefix_hash,
                    estimated_context_tokens,
                );
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

            if calls.len() > 1 {
                emit(
                    tx,
                    RuntimeEvent::Error {
                        turn_id: Some(turn_id.clone()),
                        message: format!(
                            "multi tool call turns are not supported yet (got {} calls)",
                            calls.len()
                        ),
                    },
                );
                self.abort_turn().await;
                return;
            }

            let call = calls.into_iter().next().expect("exactly one tool call");
            let payload = tool_call_payload(&call);
            emit(
                tx,
                RuntimeEvent::ToolCallStarted {
                    turn_id: turn_id.clone(),
                    tool_call_id: ToolCallId::from(call.id.clone()),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            );

            {
                let mut state = self.state.lock().await;
                state.session.push(Message::assistant_with_tool_calls(
                    text_buffer,
                    vec![payload],
                ));
            }
            self.persist().await;
            self.emit_session_updated(tx).await;

            match self.tools.run_tool_call(call.clone(), None) {
                Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                    {
                        let mut state = self.state.lock().await;
                        state.pending = Some(PendingToolCall {
                            call,
                            turn_id: turn_id.clone(),
                        });
                    }
                    emit(
                        tx,
                        RuntimeEvent::ApprovalRequired {
                            turn_id: Some(turn_id.clone()),
                            tool_call_id: Some(ToolCallId::from(request.call_id.clone())),
                            request,
                        },
                    );
                    return;
                }
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                    // Loop again: feed tool result back into the next chat turn.
                    continue;
                }
                Err(error) => {
                    emit(
                        tx,
                        runtime_error_from_tool_error(error, Some(turn_id.clone())),
                    );
                    self.abort_turn().await;
                    return;
                }
            }
        }
    }
}
