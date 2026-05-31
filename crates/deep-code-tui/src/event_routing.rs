use deep_code_agent::{AgentEvent, RuntimeEvent, ToolCallId};

use crate::active_turn::{ActiveToolCell, ActiveTurn};
use crate::app::App;
use crate::history::{HistoryCell, summarize_tool_result};

impl App {
    pub(crate) fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TurnStarted { turn_id, .. } => {
                self.active_turn = Some(ActiveTurn::new(turn_id));
                self.status = format!("Streaming from {}...", self.backend_label);
            }
            RuntimeEvent::Provider(AgentEvent::TextDelta { text }) => {
                self.push_provider_assistant(&text);
            }
            RuntimeEvent::Provider(AgentEvent::ReasoningDelta { text }) => {
                self.push_provider_reasoning(&text);
            }
            RuntimeEvent::Provider(AgentEvent::ToolCallDelta { .. }) => {
                self.status = "Receiving tool call...".to_string();
            }
            RuntimeEvent::Provider(AgentEvent::Done { .. }) => {}
            RuntimeEvent::Provider(AgentEvent::Error { message }) => {
                self.record_error(message);
            }
            RuntimeEvent::AssistantDelta { text, .. } => {
                self.push_structured_assistant(&text);
            }
            RuntimeEvent::ReasoningDelta { text, .. } => {
                self.push_structured_reasoning(&text);
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
                });
                self.status = format!("Receiving tool call: {tool_name}");
            }
            RuntimeEvent::ToolCallUpdated {
                tool_call_id,
                arguments_delta,
                ..
            } => {
                if let Some(delta) = arguments_delta {
                    self.append_active_tool_arguments(&tool_call_id, &delta);
                }
                self.status = "Receiving tool call...".to_string();
            }
            RuntimeEvent::ApprovalRequired { request, .. } => {
                self.set_active_approval(request.clone());
                let sandbox = if request.requires_sandbox {
                    "yes"
                } else {
                    "no"
                };
                self.status = format!(
                    "Approve '{}' (risk={:?}, sandbox={sandbox})? y/n",
                    request.tool_name, request.risk_level
                );
                self.pending_approval = Some(request);
                self.is_streaming = false;
                self.clear_stream_receiver();
            }
            RuntimeEvent::ApprovalResolved { decision, .. } => {
                if let Some(active) = self.active_turn.as_mut() {
                    active.pending_approval = None;
                }
                self.status = format!("Approval resolved: {decision:?}");
            }
            RuntimeEvent::CheckpointCreated { id, label } => {
                self.last_checkpoint = Some(id.0.clone());
                self.history
                    .push(HistoryCell::Checkpoint { id: id.0, label });
            }
            RuntimeEvent::WorkspaceRestored { id } => {
                self.last_checkpoint = Some(id.0.clone());
                self.history.push(HistoryCell::System {
                    text: format!("Workspace restored from checkpoint {}", id.0),
                });
                self.status = format!("Restored checkpoint {}", id.0);
            }
            RuntimeEvent::ToolResult { result } => {
                self.push_tool_result_cell(&result);
            }
            RuntimeEvent::ToolCallFinished { result, .. } => {
                self.flush_active_turn();
                self.push_tool_result_cell(&result);
            }
            RuntimeEvent::SessionUpdated {
                session_id,
                turn_count,
                compaction,
                ..
            } => {
                if let Some(session_id) = session_id {
                    self.session_id = Some(session_id.0);
                }
                if let Some(compaction) = compaction {
                    self.status = format!("Session updated: {turn_count} turn(s), {compaction}");
                }
            }
            RuntimeEvent::DiagnosticsUpdated { summary, rendered } => {
                self.history.push(HistoryCell::Diagnostics {
                    summary: summary.clone(),
                    rendered,
                });
                self.status = format!("Diagnostics: {summary}");
            }
            RuntimeEvent::CompactionApplied {
                archived_count,
                summary,
            } => {
                self.history.push(HistoryCell::System {
                    text: format!("已压缩 {archived_count} 条历史消息\n{summary}"),
                });
                self.status = format!("已压缩 {archived_count} 条历史消息");
            }
            RuntimeEvent::TurnFinished { telemetry, .. } => {
                self.flush_active_turn();
                self.last_telemetry = telemetry.clone();
                let checkpoint = self
                    .last_checkpoint
                    .as_ref()
                    .map(|id| format!(" | 回滚: /restore {id}"))
                    .unwrap_or_default();
                let session = self
                    .session_id
                    .as_ref()
                    .map(|id| format!(" | session {id}"))
                    .unwrap_or_default();
                let telemetry_note = telemetry
                    .as_ref()
                    .map(|value| crate::commands::format_turn_telemetry(value, self.cost_currency))
                    .unwrap_or_default();
                self.status = format!(
                    "就绪 - {}{}{}{telemetry_note}",
                    self.backend_label, checkpoint, session
                );
                self.is_streaming = false;
                self.clear_stream_receiver();
            }
            RuntimeEvent::Error { message, .. } => self.record_error(message),
        }
    }

    fn push_structured_assistant(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_structured_assistant(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_structured_assistant(text);
            self.active_turn = Some(active);
        }
    }

    fn push_provider_assistant(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_provider_assistant(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_provider_assistant(text);
            self.active_turn = Some(active);
        }
    }

    fn push_structured_reasoning(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_structured_reasoning(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_structured_reasoning(text);
            self.active_turn = Some(active);
        }
    }

    fn push_provider_reasoning(&mut self, text: &str) {
        if let Some(active) = self.active_turn.as_mut() {
            active.push_provider_reasoning(text);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.push_provider_reasoning(text);
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
            active.pending_approval = Some(request);
        } else {
            let mut active = ActiveTurn::new(Default::default());
            active.pending_approval = Some(request);
            self.active_turn = Some(active);
        }
    }

    fn flush_active_turn(&mut self) {
        let Some(active) = self.active_turn.take() else {
            return;
        };
        self.history.extend(active.preview_cells());
    }

    fn push_tool_result_cell(&mut self, result: &deep_code_agent::ToolResult) {
        let summary = summarize_tool_result(&result.content);
        let duplicate = matches!(
            self.history.last(),
            Some(HistoryCell::ToolResult {
                tool_name,
                status,
                summary: existing_summary,
            }) if tool_name == &result.tool_name
                && status == &result.status
                && existing_summary == &summary
        );
        if !duplicate {
            self.history.push(HistoryCell::ToolResult {
                tool_name: result.tool_name.clone(),
                status: result.status.clone(),
                summary,
            });
        }
        if deep_code_agent::is_subagent_tool(&result.tool_name) {
            self.refresh_subagent_status();
        }
    }
}
