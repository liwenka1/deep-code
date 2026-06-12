use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

pub(super) const CANCELLED_TOOL_RESULT: &str = "用户取消了本轮，该工具调用未执行 (cancelled by user)";

/// Whether "approve for the whole session" may be recorded for a tool.
/// Shell-class tools are excluded: their risk lives in the per-call
/// arguments, so a blanket session consent would be misleading.
pub(super) fn session_allowable(tool_name: &str) -> bool {
    !matches!(
        crate::execution_policy::ExecPolicy::classify_tool(tool_name),
        crate::execution_policy::ToolKind::Shell
    )
}

/// How a tool-call batch ended: every call has a recorded result, the batch
/// is parked in `RuntimeState::pending` waiting for an approval, or the user
/// cancelled and the remaining calls received synthesized results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed,
    AwaitingApproval,
    Cancelled,
}

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Execute one tool call on the blocking pool so a long-running tool does
    /// not stall the async runtime (cancellation still lands at call
    /// boundaries: the tool itself runs to completion to keep its recorded
    /// result paired with the assistant tool_call).
    async fn run_tool_blocking(
        &self,
        call: ToolCall,
        decision: Option<ApprovalDecision>,
    ) -> Result<ToolRunOutcome, ToolError> {
        let tools = Arc::clone(&self.tools);
        let name = call.name.clone();
        match tokio::task::spawn_blocking(move || tools.run_tool_call(call, decision)).await {
            Ok(outcome) => outcome,
            Err(join_error) => Err(ToolError::ExecutionFailed {
                name,
                message: format!("tool execution task failed: {join_error}"),
            }),
        }
    }

    /// Persist the session and announce the authoritative transcript change.
    /// Called once per batch boundary instead of once per tool call.
    async fn flush_session_update(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        self.persist().await;
        self.emit_session_updated(tx).await;
    }

    /// Whether a gated call may run without asking: the user session-allowed
    /// the tool earlier, or a configured `auto_allow` prefix matches. Policy
    /// hard-denials are unaffected (they short-circuit inside the registry
    /// before any decision is consulted).
    async fn auto_approval_granted(&self, tool_name: &str) -> bool {
        if self
            .config
            .approval_auto_allow
            .iter()
            .any(|prefix| !prefix.is_empty() && tool_name.starts_with(prefix))
        {
            return true;
        }
        self.state
            .lock()
            .await
            .session_approved
            .contains(tool_name)
    }

    /// Run the queued tool calls of one assistant turn in order.
    ///
    /// Every call ends with a recorded tool result message — including policy
    /// denials and execution errors — so each `tool_call` in the assistant
    /// message keeps its paired tool message (provider requirement). The only
    /// early exit is an approval request, which parks the rest of the batch.
    /// Persistence and `SessionUpdated` are flushed once per batch outcome.
    pub(super) async fn process_tool_batch(
        &self,
        mut remaining: VecDeque<ToolCall>,
        turn_id: &TurnId,
        cancel: &CancellationToken,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> BatchOutcome {
        while let Some(call) = remaining.pop_front() {
            // Cancellation takes effect at call boundaries; the in-flight
            // tool (if any) completed on the blocking pool.
            if cancel.is_cancelled() {
                remaining.push_front(call);
                self.finish_cancelled_calls(remaining, turn_id, tx).await;
                return BatchOutcome::Cancelled;
            }
            match self.run_tool_blocking(call.clone(), None).await {
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                }
                Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                    if self.auto_approval_granted(&call.name).await {
                        // Audit trail: the gate fired but a standing consent
                        // (session "a" or config auto_allow) resolved it.
                        emit(
                            tx,
                            RuntimeEvent::ApprovalResolved {
                                turn_id: Some(turn_id.clone()),
                                tool_call_id: ToolCallId::from(call.id.clone()),
                                decision: ApprovalDecision::Approved,
                            },
                        );
                        let result = match self
                            .run_tool_blocking(call.clone(), Some(ApprovalDecision::Approved))
                            .await
                        {
                            Ok(ToolRunOutcome::Result { result }) => result,
                            Ok(ToolRunOutcome::ApprovalRequired { .. }) => ToolResult::error(
                                &call,
                                "tool re-requested approval after auto-approve",
                            ),
                            Err(error) => ToolResult::error(&call, error.to_string()),
                        };
                        self.record_tool_result(&call, result, tx, turn_id.clone())
                            .await;
                        continue;
                    }
                    {
                        let mut state = self.state.lock().await;
                        state.pending = Some(PendingToolBatch {
                            current: call,
                            remaining,
                            turn_id: turn_id.clone(),
                        });
                    }
                    self.flush_session_update(tx).await;
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
        self.flush_session_update(tx).await;
        BatchOutcome::Completed
    }

    /// Finalize a batch that was parked on an approval when the user
    /// cancelled: every unresolved call gets a synthesized result so the
    /// assistant message keeps its tool_call/tool message pairing.
    pub(super) async fn finalize_cancelled_batch(
        &self,
        pending: PendingToolBatch,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let PendingToolBatch {
            current,
            mut remaining,
            turn_id,
        } = pending;
        remaining.push_front(current);
        self.finish_cancelled_calls(remaining, &turn_id, tx).await;
    }

    /// Record synthesized cancelled results for every queued call, then close
    /// the turn and emit the terminal `TurnCancelled` event.
    async fn finish_cancelled_calls(
        &self,
        calls: VecDeque<ToolCall>,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        for call in calls {
            let result = ToolResult::error(&call, CANCELLED_TOOL_RESULT);
            self.record_tool_result(&call, result, tx, turn_id.clone())
                .await;
        }
        self.flush_session_update(tx).await;
        self.finish_turn_cancelled(turn_id, tx).await;
    }

    /// The single cancellation epilogue: close the turn (persisting it) and
    /// emit the terminal `TurnCancelled` event. Every cancel path funnels
    /// through here so the ordering can never drift between call sites.
    pub(super) async fn finish_turn_cancelled(
        &self,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        self.finish_turn(None).await;
        emit(
            tx,
            RuntimeEvent::TurnCancelled {
                turn_id: turn_id.clone(),
            },
        );
    }

    pub(super) async fn handle_approval(
        &self,
        pending: PendingToolBatch,
        decision: ApprovalDecision,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let cancel = self.state.lock().await.cancel.clone();
        let PendingToolBatch {
            current,
            remaining,
            turn_id,
        } = pending;
        if cancel.is_cancelled() {
            let mut calls = remaining;
            calls.push_front(current);
            self.finish_cancelled_calls(calls, &turn_id, tx).await;
            return;
        }
        // "Approve for session" is recorded here and executes as a plain
        // approve; shell-class tools only get the one-time approval.
        let decision = if decision == ApprovalDecision::ApprovedForSession {
            if session_allowable(&current.name) {
                self.state
                    .lock()
                    .await
                    .session_approved
                    .insert(current.name.clone());
            }
            ApprovalDecision::Approved
        } else {
            decision
        };
        match self.run_tool_blocking(current.clone(), Some(decision)).await {
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
        if self.process_tool_batch(remaining, &turn_id, &cancel, tx).await == BatchOutcome::Completed
        {
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
        // Persistence and SessionUpdated are flushed once per batch boundary
        // (see process_tool_batch / finish_cancelled_calls), not per call.
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
