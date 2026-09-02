use deep_code_agent::{RuntimeEvent, ToolCallId};

use crate::active_turn::{ActiveToolCell, ActiveTurn};
use crate::app::App;
use crate::history::{HistoryCell, ToolApprovalState, summarize_tool_result};
use deep_code_agent::i18n::TextId;

impl App {
    pub(crate) fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TurnStarted { turn_id, .. } => {
                // Never drop a predecessor that streamed content but missed
                // its terminal event — flush it into history first.
                self.flush_active_turn();
                self.active_turn = Some(ActiveTurn::new(turn_id));
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
                    risk_level: None,
                    requires_sandbox: None,
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
                self.status = self.tr_with(
                    TextId::StatusApprovalResolved,
                    &[("decision", &format!("{decision:?}"))],
                );
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
                if let Some(active) = self.active_turn.as_mut() {
                    let cells = active.take_finished_tool_cells(&tool_call_id);
                    self.history.extend(cells);
                }
                self.push_tool_result_cell(&result);
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
                    metadata: Some(format!("archived={archived_count}")),
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
                self.clear_stream_receiver();
                // Fire anything the user lined up while this turn streamed —
                // but only once the drain loop is done, see the field's doc.
                self.pending_steering_flush = true;
            }
            RuntimeEvent::TurnCancelled { .. } => {
                self.flush_active_turn();
                self.history
                    .push(HistoryCell::system(self.tr(TextId::SystemTurnCancelled)));
                self.status = self.tr(TextId::StatusCancelled).to_string();
                self.pending_approval = None;
                self.is_streaming = false;
                // Cancel means "changed my mind": drop prompts queued behind
                // the abandoned turn rather than firing them at nothing.
                self.steering_queue.clear();
                self.clear_stream_receiver();
            }
            RuntimeEvent::Error { message, .. } => self.record_error(message),
        }
    }

    fn push_assistant_delta(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_assistant_delta(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_assistant_delta(text);
            self.active_turn = Some(active);
        }
    }

    fn push_reasoning_delta(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_reasoning_delta(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_reasoning_delta(text);
            self.active_turn = Some(active);
        }
    }

    fn upsert_active_tool(&mut self, cell: ActiveToolCell) {
        if let Some(active) = self.active_turn.as_mut() {
            active.upsert_tool(cell);
        } else {
            let mut active = ActiveTurn::new(Default::default());
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
            let mut active = ActiveTurn::new(Default::default());
            active.append_tool_arguments(tool_call_id, delta);
            self.active_turn = Some(active);
        }
    }

    fn set_active_approval(&mut self, request: deep_code_agent::ApprovalRequest) {
        if let Some(active) = self.active_turn.as_mut() {
            active.mark_approval_required(&request);
            active.pending_approval = Some(request);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.mark_approval_required(&request);
            active.pending_approval = Some(request);
            self.active_turn = Some(active);
        }
    }

    pub(crate) fn flush_active_turn(&mut self) {
        let Some(active) = self.active_turn.take() else {
            return;
        };
        self.history.extend(active.preview_cells());
    }

    fn push_tool_result_cell(&mut self, result: &deep_code_agent::ToolResult) {
        // Exactly one ToolCallFinished per tool call — no dedup needed.
        self.history.push(HistoryCell::ToolResult {
            tool_name: result.tool_name.clone(),
            status: result.status,
            summary: summarize_tool_result(&result.content),
        });
        if deep_code_agent::is_subagent_tool(&result.tool_name) {
            self.refresh_subagent_status();
        }
    }
}
