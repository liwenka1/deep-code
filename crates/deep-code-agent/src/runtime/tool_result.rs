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
    ApprovalDecision, ToolCall, ToolCx, ToolError, ToolResult, ToolResultStatus, ToolRunOutcome,
};

pub(super) const CANCELLED_TOOL_RESULT: &str =
    "用户取消了本轮，该工具调用未执行 (cancelled by user)";

/// Tool-call execution failures within a single turn that latch cascade
/// escalation (Flash → Pro for the rest of the session). Two mirrors the
/// "2–3 failed self-corrections, then escalate" rule of thumb without waiting
/// so long that a whole turn is wasted flailing on the weak model.
const CASCADE_ESCALATE_TOOL_ERRORS: u32 = 2;

/// Whether "approve for the whole session" may be recorded for a tool.
/// Shell-class tools are excluded: their risk lives in the per-call
/// arguments, so a blanket session consent would be misleading. The `job`
/// tool is excluded for the same reason — a session-allow granted on a
/// `cancel` prompt must not blanket-approve future `action=start` commands.
pub(super) fn session_allowable(tool_name: &str) -> bool {
    !matches!(
        crate::execution_policy::ExecPolicy::classify_tool(tool_name),
        crate::execution_policy::ToolKind::Shell | crate::execution_policy::ToolKind::Job
    )
}

/// Leading program of a *simple* shell command, lowercased — e.g. `cargo` from
/// `cargo test --all`. Returns `None` for non-shell calls and for
/// compound/substitution/redirection commands, which are never matched by the
/// session shell allowlist (they keep prompting). This is what `a` records and
/// later matches against, so a trusted `cargo` can never smuggle a chained
/// `cargo x && rm -rf /` past the gate.
pub(super) fn session_shell_prefix(call: &ToolCall) -> Option<String> {
    let command_bearing = match crate::execution_policy::ExecPolicy::classify_tool(&call.name) {
        crate::execution_policy::ToolKind::Shell => true,
        // Only `job action=start` carries a command; status/tail/cancel
        // approvals must not record a shell prefix.
        crate::execution_policy::ToolKind::Job => {
            call.arguments.get("action").and_then(|value| value.as_str()) == Some("start")
        }
        _ => false,
    };
    if !command_bearing {
        return None;
    }
    let command = call
        .arguments
        .get("command")
        .and_then(|value| value.as_str())?;
    let command = command.trim();
    if command.is_empty() || command.contains(['&', '|', ';', '\n', '`', '<', '>', '(', ')', '$']) {
        return None;
    }
    let token = command.split_whitespace().next()?;
    if token.contains('=') {
        return None; // leading `FOO=bar` env assignment, not a plain program
    }
    Some(token.to_ascii_lowercase())
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

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Execute one tool call with the turn's cancellation token and a progress
    /// bridge attached. Cancellation still lands at call boundaries: the tool
    /// runs to completion so its recorded result stays paired with the
    /// assistant tool_call (tools observe the token cooperatively for now).
    async fn run_tool(
        &self,
        call: &ToolCall,
        decision: Option<ApprovalDecision>,
        cancel: &CancellationToken,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<ToolRunOutcome, ToolError> {
        let cx = ToolCx::new()
            .with_cancel(cancel.clone())
            .with_update_fn(tool_progress_fn(tx, turn_id, call));
        let plan = self.tools.evaluate_tool(call);
        self.tools
            .run_tool_call_with_plan(call, decision, plan, cx)
            .await
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
    async fn auto_approval_granted(&self, call: &ToolCall) -> bool {
        if self
            .config
            .approval_auto_allow
            .iter()
            .any(|prefix| !prefix.is_empty() && call.name.starts_with(prefix))
        {
            return true;
        }
        let state = self.state.lock().await;
        if state.session_approved.contains(&call.name) {
            return true;
        }
        // Shell isn't blanket session-approvable by name; trust at command
        // granularity instead ("a" remembered `cargo`, `git`, …).
        match session_shell_prefix(call) {
            Some(prefix) => state.session_trusted_shell_prefixes.contains(&prefix),
            None => false,
        }
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
            match self.run_tool(&call, None, cancel, turn_id, tx).await {
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx, turn_id.clone())
                        .await;
                }
                Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                    if self.auto_approval_granted(&call).await {
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
            } else if let Some(prefix) = session_shell_prefix(&current) {
                // Shell: remember this command's program for the session so
                // repeated `cargo`/`git`/… stop prompting (compound commands
                // still prompt — `session_shell_prefix` returns None for them).
                self.state
                    .lock()
                    .await
                    .session_trusted_shell_prefixes
                    .insert(prefix);
            }
            ApprovalDecision::Approved
        } else {
            decision
        };
        match self
            .run_tool(&current, Some(decision), &cancel, &turn_id, tx)
            .await
        {
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
        if self
            .process_tool_batch(remaining, &turn_id, &cancel, tx)
            .await
            == BatchOutcome::Completed
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
            // Model history gets a size-bounded copy; the event stream and the
            // persisted TurnRecord keep the full output.
            let trimmed = truncate_tool_output(&result.content);
            if !state
                .session
                .record_tool_result(&result.call_id, trimmed, result.status)
            {
                // Should be unreachable: the assistant entry carrying this
                // call was pushed before the batch ran.
                eprintln!(
                    "warn: tool result {} had no pending exchange to attach to",
                    result.call_id
                );
            }
            if let Some(turn) = state.current_turn.as_mut() {
                turn.tool_results.push(result.clone());
            }
            // Cascade signal: a genuine execution failure means the model
            // fumbled this tool call. Denials carry their own status and user
            // cancellations carry a known marker, so neither counts. Enough
            // fumbles in one turn latch escalation onto Pro (sticky for the
            // session); the latch is read by the next turn's router.
            if result.status == ToolResultStatus::Error && result.content != CANCELLED_TOOL_RESULT {
                state.turn_tool_errors += 1;
                if state.turn_tool_errors >= CASCADE_ESCALATE_TOOL_ERRORS
                    && !state.cascade_escalated
                {
                    state.cascade_escalated = true;
                    // Mark the triggering turn so telemetry can surface the
                    // escalation now, not just on the next (Pro) turn.
                    state.cascade_triggered_this_turn = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            arguments: json!({ "command": command }),
        }
    }

    fn job_start(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "job".to_string(),
            arguments: json!({ "action": "start", "command": command }),
        }
    }

    #[test]
    fn shell_prefix_covers_job_start_but_not_other_actions() {
        assert_eq!(
            session_shell_prefix(&job_start("cargo test --all")),
            Some("cargo".to_string())
        );
        let cancel = ToolCall {
            id: "c1".to_string(),
            name: "job".to_string(),
            arguments: json!({ "action": "cancel", "job_id": "job_1" }),
        };
        assert_eq!(session_shell_prefix(&cancel), None);
    }

    #[test]
    fn shell_prefix_extracts_leading_program() {
        assert_eq!(
            session_shell_prefix(&shell("cargo test --all")),
            Some("cargo".to_string())
        );
        assert_eq!(
            session_shell_prefix(&shell("  Git Status  ")),
            Some("git".to_string())
        );
    }

    #[test]
    fn shell_prefix_rejects_compound_and_substitution() {
        // The guard against `cargo … && rm -rf /` riding a trusted `cargo`.
        for command in [
            "cargo test && rm -rf /",
            "ls | grep foo",
            "a; b",
            "echo `whoami`",
            "echo $(id)",
            "cat < file",
            "echo x > y",
            "FOO=bar cargo test",
            "",
        ] {
            assert_eq!(
                session_shell_prefix(&shell(command)),
                None,
                "must not trust: {command:?}"
            );
        }
    }

    #[test]
    fn shell_prefix_is_none_for_non_shell_tools() {
        let call = ToolCall {
            id: "c1".to_string(),
            name: "write_file".to_string(),
            arguments: json!({ "path": "x", "content": "y" }),
        };
        assert_eq!(session_shell_prefix(&call), None);
    }
}
