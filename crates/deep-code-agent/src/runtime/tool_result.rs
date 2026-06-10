use std::collections::VecDeque;

use tokio::sync::mpsc;

use crate::client::LlmClient;
use crate::lsp::{is_edit_tool, render_blocks, summarize_blocks};
use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::runtime::AgentRuntime;
use crate::runtime::diagnostics::append_diagnostics;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::state::PendingToolBatch;
use crate::tool::{
    ApprovalDecision, ToolCall, ToolError, ToolResult, ToolResultStatus, ToolRunOutcome,
};

/// How a tool-call batch ended: either every call has a recorded result, or
/// the batch is parked in `RuntimeState::pending` waiting for an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed,
    AwaitingApproval,
}

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Run the queued tool calls of one assistant turn in order.
    ///
    /// Every call ends with a recorded tool result message — including policy
    /// denials and execution errors — so each `tool_call` in the assistant
    /// message keeps its paired tool message (provider requirement). The only
    /// early exit is an approval request, which parks the rest of the batch.
    pub(super) async fn process_tool_batch(
        &self,
        mut remaining: VecDeque<ToolCall>,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> BatchOutcome {
        while let Some(call) = remaining.pop_front() {
            match self.tools.run_tool_call(call.clone(), None) {
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                }
                Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                    {
                        let mut state = self.state.lock().await;
                        state.pending = Some(PendingToolBatch {
                            current: call,
                            remaining,
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
                    return BatchOutcome::AwaitingApproval;
                }
                Err(error) => {
                    let result = ToolResult::error(&call, error.to_string());
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                }
            }
        }
        BatchOutcome::Completed
    }

    pub(super) async fn handle_approval(
        &self,
        pending: PendingToolBatch,
        decision: ApprovalDecision,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let PendingToolBatch {
            current,
            remaining,
            turn_id,
        } = pending;
        match self.tools.run_tool_call(current.clone(), Some(decision)) {
            Ok(ToolRunOutcome::Result { result }) => {
                self.record_tool_result(&current, result, tx, turn_id.clone())
                    .await;
            }
            Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                {
                    let mut state = self.state.lock().await;
                    state.pending = Some(PendingToolBatch {
                        current,
                        remaining,
                        turn_id: turn_id.clone(),
                    });
                }
                emit(
                    tx,
                    RuntimeEvent::ApprovalRequired {
                        turn_id: Some(turn_id),
                        tool_call_id: Some(ToolCallId::from(request.call_id.clone())),
                        request,
                    },
                );
                return;
            }
            Err(error) => {
                let result = ToolResult::error(&current, error.to_string());
                self.record_tool_result(&current, result, tx, turn_id.clone())
                    .await;
            }
        }

        // Resolved call recorded; drain the rest of the batch, then resume the
        // loop to feed all tool results into the next chat turn.
        if self.process_tool_batch(remaining, &turn_id, tx).await == BatchOutcome::Completed {
            self.run_loop(tx).await;
        }
    }

    pub(super) async fn record_tool_result(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        turn_id: TurnId,
    ) {
        if result.status == ToolResultStatus::Success
            && is_edit_tool(&call.name)
            && let Some(lsp) = self.lsp.as_ref()
        {
            let blocks = lsp.collect_for_edit(&call.name, &call.arguments).await;
            if !blocks.is_empty() {
                let rendered = render_blocks(&blocks);
                let summary = summarize_blocks(&blocks);
                result.content = append_diagnostics(&result.content, &rendered);
                emit(
                    tx,
                    RuntimeEvent::DiagnosticsUpdated {
                        summary: summary.clone(),
                        rendered,
                    },
                );
            }
        }

        {
            let mut state = self.state.lock().await;
            state.session.push(result.to_message());
            if let Some(turn) = state.current_turn.as_mut() {
                turn.tool_results.push(result.clone());
            }
        }
        self.persist().await;
        self.emit_session_updated(tx).await;
        emit(
            tx,
            RuntimeEvent::ToolCallFinished {
                turn_id: Some(turn_id),
                tool_call_id: ToolCallId::from(call.id.clone()),
                result: result.clone(),
            },
        );
        emit(tx, RuntimeEvent::ToolResult { result });
    }
}

pub(super) fn tool_call_payload(call: &ToolCall) -> ToolCallPayload {
    // Compact form keeps history small and matches typical OpenAI-style
    // assistant payloads. We don't try to preserve the exact bytes the model
    // produced because we already parsed them through `ToolCallAccumulator`.
    let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string());
    ToolCallPayload {
        id: call.id.clone(),
        call_type: "function".to_string(),
        function: ToolCallFunctionPayload {
            name: call.name.clone(),
            arguments,
        },
    }
}

pub(super) fn runtime_error_from_tool_error(
    error: ToolError,
    turn_id: Option<TurnId>,
) -> RuntimeEvent {
    RuntimeEvent::Error {
        turn_id,
        message: error.to_string(),
    }
}
