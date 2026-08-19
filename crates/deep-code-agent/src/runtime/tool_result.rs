use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::failure_class::record_failure_signals;
use crate::runtime::state::PendingToolBatch;
use crate::tool::{ApprovalDecision, ToolCall, ToolCx, ToolError, ToolResult, ToolRunOutcome};

/// Whether a call may run concurrently inside a tool batch. Only sub-agent
/// calls qualify: the execution policy allows them without approval, and each
/// spawns an isolated child runtime, so a batch of them shares no mutable
/// state and never parks the batch on a human-in-the-loop pause.
fn is_parallel_safe(call: &ToolCall) -> bool {
    matches!(
        crate::execution_policy::ExecPolicy::classify_tool(&call.name),
        crate::execution_policy::ToolKind::SubAgent
    )
}

pub(super) const CANCELLED_TOOL_RESULT: &str =
    "用户取消了本轮，该工具调用未执行 (cancelled by user)";

/// How a tool-call batch ended: every call has a recorded result, the batch
/// is parked in `RuntimeState::pending` waiting for an approval, or the user
/// cancelled and the remaining calls received synthesized results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed,
    AwaitingApproval,
    Cancelled,
}

/// Progress bridge: tool `cx.update(..)` calls become `ToolCallProgress`
/// runtime events attributed to the emitting call.
fn tool_progress_fn(
    tx: &mpsc::UnboundedSender<RuntimeEvent>,
    turn_id: &TurnId,
    call: &ToolCall,
) -> crate::tool::ToolUpdateFn {
    let tx = tx.clone();
    let turn_id = turn_id.clone();
    let tool_call_id = ToolCallId::from(call.id.clone());
    let tool_name = call.name.clone();
    Arc::new(move |update| {
        let _ = tx.send(RuntimeEvent::ToolCallProgress {
            turn_id: Some(turn_id.clone()),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            update,
        });
    })
}

impl AgentRuntime {
    /// Execute one tool call with the turn's cancellation token and a progress
    /// bridge attached. Cancellation still lands at call boundaries: the tool
    /// runs to completion so its recorded result stays paired with the
    /// assistant tool_call (tools observe the token cooperatively for now).
    pub(super) async fn run_tool(
        &self,
        call: &ToolCall,
        decision: Option<ApprovalDecision>,
        cancel: &CancellationToken,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<ToolRunOutcome, ToolError> {
        // A sink for spend a tool incurs out-of-band (a sub-agent's own request
        // spend never reaches this turn's telemetry). Folded into the session
        // totals after the tool returns.
        let spend_sink =
            std::sync::Arc::new(std::sync::Mutex::new(crate::tool::ToolSpend::default()));
        let cx = ToolCx::new()
            .with_cancel(cancel.clone())
            .with_update_fn(tool_progress_fn(tx, turn_id, call))
            .with_spend_sink(std::sync::Arc::clone(&spend_sink));
        let plan = self.tools.evaluate_tool(call);
        let outcome = self
            .tools
            .run_tool_call_with_plan(call, decision, plan, cx)
            .await;
        let reported = *spend_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !reported.is_zero() {
            // Cache counters fold alongside the cost for the same reason
            // `record_classifier_cost` folds them: the session hit-rate and
            // savings must cover every request billed to the session.
            let mut state = self.state.lock().await;
            state.session_cost.usd += reported.cost.usd;
            state.session_cost.cny += reported.cost.cny;
            state.session_cache_hit_tokens += reported.cache_hit_tokens;
            state.session_cache_miss_tokens += reported.cache_miss_tokens;
            state.session_cache_savings.usd += reported.cache_savings.usd;
            state.session_cache_savings.cny += reported.cache_savings.cny;
        }
        outcome
    }

    /// Persist the session and announce the authoritative transcript change.
    /// Called once per batch boundary instead of once per tool call.
    async fn flush_session_update(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        self.persist().await;
        self.emit_session_updated(tx).await;
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
            // Batch-internal parallelism: a run of independent, approval-free
            // `agent` calls issued in one turn executes concurrently — this is
            // what makes "issue several agent calls to run children in
            // parallel" true. Results are still recorded in issue order so the
            // wire transcript (and prefix cache) stays deterministic. Only
            // sub-agent calls qualify: they never park on approval and share no
            // mutable state, so concurrency can't race the approval machinery.
            if is_parallel_safe(&call) && remaining.front().is_some_and(is_parallel_safe) {
                let mut group = vec![call];
                while remaining.front().is_some_and(is_parallel_safe) {
                    group.push(remaining.pop_front().expect("front just checked"));
                }
                let outcomes = futures_util::future::join_all(
                    group
                        .iter()
                        .map(|call| self.run_tool(call, None, cancel, turn_id, tx)),
                )
                .await;
                for (call, outcome) in group.iter().zip(outcomes) {
                    let result = match outcome {
                        Ok(ToolRunOutcome::Result { result }) => result,
                        Ok(ToolRunOutcome::ApprovalRequired { .. }) => ToolResult::error(
                            call,
                            "parallel sub-agent unexpectedly requested approval",
                        ),
                        Err(error) => ToolResult::error(call, error.to_string()),
                    };
                    self.record_tool_result(call, result, tx, turn_id.clone())
                        .await;
                }
                continue;
            }
            match self.run_tool(&call, None, cancel, turn_id, tx).await {
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                }
                Ok(ToolRunOutcome::ApprovalRequired { mut request }) => {
                    if self.auto_approval_granted(&call, &request, cancel).await {
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
                            .run_tool(&call, Some(ApprovalDecision::Approved), cancel, turn_id, tx)
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
                    // File I/O for a bounded diff at a human-in-the-loop
                    // pause: cheap relative to the wait, so no spawn_blocking.
                    request.preview = self.approval_preview(&call);
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
    pub(super) async fn finish_cancelled_calls(
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
        self.finish_turn(turn_id).await;
        emit(
            tx,
            RuntimeEvent::TurnCancelled {
                turn_id: turn_id.clone(),
            },
        );
    }

    pub(super) async fn record_tool_result(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        turn_id: TurnId,
    ) {
        self.attach_edit_diagnostics(call, &mut result, tx).await;

        {
            let mut state = self.state.lock().await;
            // Size-bounded copy, and the only one that gets persisted — the
            // untruncated output goes out on the event stream and is not stored
            // anywhere (the second, lossless copy in `TurnRecord.tool_results`
            // was removed to stop O(session²) write amplification).
            let trimmed = truncate_tool_output(&result.content);
            if !state
                .session
                .record_tool_result(&result.call_id, trimmed, result.status)
            {
                // Should be unreachable: the assistant entry carrying this
                // call was pushed before the batch ran.
                emit(
                    tx,
                    RuntimeEvent::Warning {
                        message: format!(
                            "tool result {} had no pending exchange to attach to",
                            result.call_id
                        ),
                    },
                );
            }
            record_failure_signals(&mut state, call, &result);
        }
        // Persistence and SessionUpdated are flushed once per batch boundary
        // (see process_tool_batch / finish_cancelled_calls), not per call.
        emit(
            tx,
            RuntimeEvent::ToolCallFinished {
                turn_id: Some(turn_id),
                tool_call_id: ToolCallId::from(call.id.clone()),
                result,
            },
        );
    }
}

const TOOL_OUTPUT_BUDGET: usize = 12_000;
const TOOL_OUTPUT_HEAD: usize = 4_000;
const TOOL_OUTPUT_TAIL: usize = 4_000;

/// Bound a tool result before it re-enters the model context: oversized
/// outputs keep their head and tail with a marker for the elided middle, so
/// the next request stays small without losing the most relevant ends.
/// Counts by `char` to stay UTF-8 safe.
pub(super) fn truncate_tool_output(content: &str) -> String {
    let total = content.chars().count();
    if total <= TOOL_OUTPUT_BUDGET {
        return content.to_string();
    }
    let head: String = content.chars().take(TOOL_OUTPUT_HEAD).collect();
    let tail: String = content.chars().skip(total - TOOL_OUTPUT_TAIL).collect();
    let elided = total - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL;
    format!("{head}\n\n...[省略 {elided} 字符 / {elided} chars truncated]...\n\n{tail}")
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
