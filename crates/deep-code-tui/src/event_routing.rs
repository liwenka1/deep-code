use deep_code_agent::{RuntimeEvent, ToolCallId};

use crate::active_turn::{ActiveToolCell, ActiveTurn};
use crate::app::App;
use crate::history::{HistoryCell, ToolApprovalState, folded_entry, summarize_tool_result};
use deep_code_agent::i18n::TextId;

impl App {
    pub(crate) fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TurnStarted { .. } => {
                // Never drop a predecessor that streamed content but missed
                // its terminal event — flush it into history first.
                self.flush_active_turn();
                self.active_turn = Some(ActiveTurn::default());
                self.status = self.tr_with(
                    TextId::StatusStreamingFrom,
                    &[("backend", &self.backend_label)],
                );
            }
            RuntimeEvent::UserMessageInjected { text, .. } => {
                // The steered prompt is already in the session (the runtime
                // recorded it before emitting), so this arm is the UI catching
                // up. Flush what the turn streamed so far into history first:
                // without it the user's cell would render ABOVE output that
                // preceded it. The reply genuinely was split in two by the
                // user's message, and this is what makes the transcript say so.
                self.flush_active_turn();
                self.history.push(HistoryCell::user(text));
                // Drop the matching pending entry. FIFO, the same order the
                // runtime drained in — the message is an ordinary user cell now,
                // no longer a promise the composer is showing.
                if !self.steering_queue.is_empty() {
                    self.steering_queue.remove(0);
                }
                self.status = self.tr_with(
                    TextId::StatusStreamingFrom,
                    &[("backend", &self.backend_label)],
                );
            }
            RuntimeEvent::AssistantDelta { text, .. } => {
                self.push_assistant_delta(&text);
            }
            RuntimeEvent::ReasoningDelta { text, .. } => {
                self.push_reasoning_delta(&text);
            }
            RuntimeEvent::ToolCallStarted {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.upsert_active_tool(ActiveToolCell {
                    tool_call_id,
                    tool_name: tool_name.clone(),
                    arguments: arguments.to_string(),
                    approval: ToolApprovalState::NotRequired,
                    live_output: Default::default(),
                    started_at: std::time::Instant::now(),
                });
                self.status =
                    self.tr_with(TextId::StatusToolCallReceiving, &[("tool", &tool_name)]);
            }
            RuntimeEvent::ToolCallUpdated {
                tool_call_id,
                arguments_delta,
                ..
            } => {
                if let Some(delta) = arguments_delta {
                    self.append_active_tool_arguments(&tool_call_id, &delta);
                }
                self.status = self.tr(TextId::StatusToolCallReceivingArgs).to_string();
            }
            RuntimeEvent::ToolCallProgress {
                tool_call_id,
                tool_name,
                update,
                ..
            } => {
                self.append_active_tool_output(&tool_call_id, &update.text);
                self.status = self.tr_with(TextId::StatusToolRunning, &[("tool", &tool_name)]);
            }
            RuntimeEvent::ApprovalRequired { .. } if self.cancel_requested => {
                // Esc landed while this request was in flight: the runtime took
                // the parked batch out from under it and finalized the
                // cancellation on the receiver `cancel_turn` returns, which the
                // UI does not pump (see `cancel_streaming_turn`). The request is
                // for a turn that no longer exists; parking it drew a panel
                // whose every answer the runtime could only drop in silence.
                self.finish_turn_cancelled();
            }
            RuntimeEvent::ApprovalRequired { request, .. } => {
                self.set_active_approval(request.clone());
                let sandbox = if request.requires_sandbox {
                    self.tr(TextId::WordYes)
                } else {
                    self.tr(TextId::WordNo)
                };
                let risk = self.tr(request.risk_level.text_id());
                self.status = self.tr_with(
                    TextId::StatusApprovalPrompt,
                    &[
                        ("tool", &request.tool_name),
                        ("risk", risk),
                        ("sandbox", sandbox),
                    ],
                );
                self.park_approval(request);
                self.clear_stream_receiver();
            }
            RuntimeEvent::ApprovalResolved { decision, .. } => {
                if let Some(active) = self.active_turn.as_mut() {
                    active.resolve_approval(decision);
                }
                let label = self.tr(decision.text_id());
                self.status = self.tr_with(TextId::StatusApprovalResolved, &[("decision", label)]);
            }
            RuntimeEvent::CheckpointCreated { id, label } => {
                self.last_checkpoint = Some(id.0.clone());
                self.history
                    .push(HistoryCell::Checkpoint { id: id.0, label });
            }
            RuntimeEvent::WorkspaceRestored { id } => {
                self.last_checkpoint = Some(id.0.clone());
                self.history.push(HistoryCell::System {
                    text: self.tr_with(TextId::SystemWorkspaceRestored, &[("id", &id.0)]),
                });
                self.status = self.tr_with(TextId::StatusRestored, &[("id", &id.0)]);
            }
            RuntimeEvent::RootGranted { path, .. } => {
                // Keep the TUI's own grant list in sync: `/add-dir` relaunches
                // pass `self.extra_roots` back to the launcher, so a grant the
                // RUNTIME performed must land here too or the next relaunch
                // would forget it (the union with the session record is the
                // backstop, this keeps the display honest right now).
                let granted = std::path::PathBuf::from(&path);
                if !self.extra_roots.contains(&granted) {
                    self.extra_roots.push(granted);
                }
                self.history.push(HistoryCell::System {
                    text: self.tr_with(TextId::SystemRootGranted, &[("path", &path)]),
                });
            }
            RuntimeEvent::ToolCallFinished {
                tool_call_id,
                result,
                ..
            } => {
                // Flush only the finished tool so cells of other calls in the
                // same multi-tool batch keep streaming in the active turn.
                let finished = self
                    .active_turn
                    .as_mut()
                    .map(|active| active.take_finished_tool(&tool_call_id))
                    .unwrap_or_default();
                self.history.extend(finished.prose);
                // Computed once, for the two consumers below: summarising scans
                // the whole tool content, so doing it per-decision would pay it
                // twice on every call.
                let summary = summarize_tool_result(&result.content);
                // A call that brought diagnostics along is shown in full: today
                // their cell is pushed BETWEEN the call and its result, so
                // folding the call would either reorder them or drop them
                // outright — and silently dropping a reader's only copy of a
                // type error is not a thing this may ever do.
                //
                // They are in hand only because the runtime drains them with
                // whichever call finishes next, which is not necessarily the
                // edit they describe — so this is a real case, not a
                // theoretical one.
                let entry = if finished.diagnostics.is_empty() {
                    finished
                        .call
                        .as_ref()
                        .and_then(|call| foldable_call(call, &result.status, &summary))
                } else {
                    None
                };
                match (finished.call, entry) {
                    (Some(_), Some(entry)) => match self.history.last_mut() {
                        Some(HistoryCell::ToolBatch { entries, .. }) => entries.push(entry),
                        _ => self.history.push(HistoryCell::ToolBatch {
                            entries: vec![entry],
                            expanded: false,
                        }),
                    },
                    (call, _) => {
                        if let Some(call) = call {
                            self.history.push(call);
                        }
                        self.history.extend(finished.diagnostics);
                        self.push_tool_result_cell(&result, summary);
                    }
                }
            }
            RuntimeEvent::SessionUpdated {
                session_id,
                turn_count,
                compaction,
                save_error,
                ..
            } => {
                if let Some(session_id) = session_id {
                    self.session_id = Some(session_id.0);
                }
                match save_error {
                    Some(error) => {
                        // Surface once per failure episode; the status line
                        // keeps warning until a save succeeds again.
                        if !self.save_error_notified {
                            self.history.push(HistoryCell::system(
                                self.tr_with(TextId::SystemSaveFailed, &[("error", &error)]),
                            ));
                            self.save_error_notified = true;
                        }
                        self.status = self.tr_with(TextId::StatusSaveFailed, &[("error", &error)]);
                    }
                    None => {
                        if self.save_error_notified {
                            self.history
                                .push(HistoryCell::system(self.tr(TextId::SystemSaveRecovered)));
                            self.save_error_notified = false;
                        }
                        if let Some(compaction) = compaction {
                            self.status = self.tr_with(
                                TextId::StatusSessionUpdated,
                                &[
                                    ("turns", &turn_count.to_string()),
                                    ("compaction", &compaction),
                                ],
                            );
                        }
                    }
                }
            }
            RuntimeEvent::DiagnosticsUpdated { summary, rendered } => {
                if let Some(active) = self.active_turn.as_mut() {
                    active.push_diagnostics(summary.clone(), rendered);
                } else {
                    self.history.push(HistoryCell::Diagnostics {
                        summary: summary.clone(),
                        rendered,
                    });
                }
                self.status = self.tr_with(TextId::StatusDiagnostics, &[("summary", &summary)]);
            }
            RuntimeEvent::CompactionApplied {
                archived_count,
                summary,
            } => {
                self.history.push(HistoryCell::Compaction {
                    archived_entries: archived_count,
                    // The automatic path fires on a threshold, so "did it buy
                    // anything" is not in question — and `CompactionApplied`
                    // carries no token counts to answer it with anyway.
                    context_tokens: None,
                    summary: summary.clone(),
                });
                self.status = self.tr_with(
                    TextId::StatusCompacted,
                    &[("count", &archived_count.to_string())],
                );
            }
            RuntimeEvent::Warning { message } => {
                self.history.push(HistoryCell::system(
                    self.tr_with(TextId::SystemWarning, &[("message", &message)]),
                ));
            }
            RuntimeEvent::TurnFinished { telemetry, .. } => {
                self.flush_active_turn();
                self.last_telemetry = telemetry.clone();
                // The durable frame (mode/backend/session/telemetry) comes
                // from `status_line()`; keep only the rollback hint here so
                // nothing shows twice.
                self.status = self
                    .last_checkpoint
                    .as_ref()
                    .map(|id| {
                        self.tr_with(TextId::StatusRollbackHint, &[("id", id)])
                            .trim_start_matches(" | ")
                            .to_string()
                    })
                    .unwrap_or_default();
                self.is_streaming = false;
                self.cancel_requested = false;
                self.clear_stream_receiver();
                // Fire anything the user lined up while this turn streamed —
                // but only once the drain loop is done, see the field's doc.
                self.pending_steering_flush = true;
            }
            RuntimeEvent::TurnCancelled { .. } => self.finish_turn_cancelled(),
            RuntimeEvent::Error { message, .. } => self.record_error(message),
        }
    }

    /// The turn ended by cancellation — either the runtime said so
    /// (`TurnCancelled`), or an `ApprovalRequired` arrived for a turn the user
    /// had already cancelled (see `apply_runtime_event`).
    pub(crate) fn finish_turn_cancelled(&mut self) {
        // Before the flush, not after: the request this turn was parked on will
        // never be answered, and the flush is what carries its cell into the
        // transcript — with a `pending` badge that would outlive the question.
        if let Some(active) = self.active_turn.as_mut() {
            active.abandon_unanswered_approvals();
        }
        self.flush_active_turn();
        self.history
            .push(HistoryCell::system(self.tr(TextId::SystemTurnCancelled)));
        self.status = self.tr(TextId::StatusCancelled).to_string();
        self.pending_approval = None;
        self.cancel_requested = false;
        self.is_streaming = false;
        self.clear_stream_receiver();
        // The cancel itself already emptied the queue, synchronously
        // (`cancel_streaming_turn`) — "changed my mind" was honoured there.
        // Whatever is queued *now* was typed after Esc, while the runtime was
        // still winding the turn down: the user's next prompt, which must fire
        // once the cancel has landed exactly as it would after `TurnFinished`.
        // Clearing again here silently dropped it after the composer had
        // confirmed it as queued.
        self.pending_steering_flush = true;
    }

    fn push_assistant_delta(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_assistant_delta(text);
        } else {
            let mut active = ActiveTurn::default();
            active.push_assistant_delta(text);
            self.active_turn = Some(active);
        }
    }

    fn push_reasoning_delta(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_reasoning_delta(text);
        } else {
            let mut active = ActiveTurn::default();
            active.push_reasoning_delta(text);
            self.active_turn = Some(active);
        }
    }

    fn upsert_active_tool(&mut self, cell: ActiveToolCell) {
        if let Some(active) = self.active_turn.as_mut() {
            active.upsert_tool(cell);
        } else {
            let mut active = ActiveTurn::default();
            active.upsert_tool(cell);
            self.active_turn = Some(active);
        }
    }

    fn append_active_tool_output(&mut self, tool_call_id: &ToolCallId, text: &str) {
        // No fallback cell: progress without a started tool call cannot be
        // attributed meaningfully, and the final result still lands.
        if let Some(active) = self.active_turn.as_mut() {
            active.append_tool_output(tool_call_id, text);
        }
    }

    fn append_active_tool_arguments(&mut self, tool_call_id: &ToolCallId, delta: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.append_tool_arguments(tool_call_id, delta);
        } else {
            let mut active = ActiveTurn::default();
            active.append_tool_arguments(tool_call_id, delta);
            self.active_turn = Some(active);
        }
    }

    fn set_active_approval(&mut self, request: deep_code_agent::ApprovalRequest) {
        if let Some(active) = self.active_turn.as_mut() {
            active.mark_approval_required(&request);
            active.pending_approval = Some(request);
        } else {
            let mut active = ActiveTurn::default();
            active.mark_approval_required(&request);
            active.pending_approval = Some(request);
            self.active_turn = Some(active);
        }
    }

    pub(crate) fn flush_active_turn(&mut self) {
        let Some(active) = self.active_turn.take() else {
            return;
        };
        self.history.extend(active.flushed_cells());
    }

    fn push_tool_result_cell(&mut self, result: &deep_code_agent::ToolResult, summary: String) {
        // Exactly one ToolCallFinished per tool call — no dedup needed.
        self.history.push(HistoryCell::ToolResult {
            status: result.status,
            summary,
        });
        if deep_code_agent::is_subagent_tool(&result.tool_name) {
            self.refresh_subagent_status();
        }
    }
}

/// The batch entry a finished call would contribute, or `None` when the call
/// has to be shown in full.
///
/// Thin adapter over [`folded_entry`], so the policy itself has exactly one
/// home and both the live path and the resume path read it from there.
fn foldable_call(
    call: &HistoryCell,
    status: &deep_code_agent::ToolResultStatus,
    summary: &str,
) -> Option<crate::history::ToolBatchEntry> {
    let HistoryCell::ToolCall {
        tool_name,
        arguments,
        approval,
        ..
    } = call
    else {
        return None;
    };
    folded_entry(tool_name, arguments, *approval, status, summary)
}
