use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{PermissionMode, RiskLevel, accept_edits_approvable, command_shape};
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH};

use crate::client::LlmClient;
use crate::lsp::{is_edit_tool, render_blocks, summarize_blocks};
use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::runtime::AgentRuntime;
use crate::runtime::diagnostics::append_diagnostics;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::state::PendingToolBatch;
use crate::tool::{
    ApprovalDecision, ApprovalRequest, ToolCall, ToolCx, ToolError, ToolResult, ToolResultStatus,
    ToolRunOutcome,
};

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

/// Identity key of a *simple* shell command — e.g. `git status` from
/// `git status -s`, or `cargo test` from `cargo test --all`. Returns `None` for
/// non-shell calls and for compound/substitution/redirection commands, which
/// are never matched by the session shell allowlist (they keep prompting).
///
/// This is what "approve for session" records and later matches against. Using
/// the command identity rather than the bare program means approving
/// `git status` does NOT blanket-approve `git push`: the two resolve to
/// different keys, so a chained or sibling subcommand can't ride a prior
/// consent past the gate.
pub(super) fn session_shell_prefix(call: &ToolCall) -> Option<String> {
    let command_bearing = match crate::execution_policy::ExecPolicy::classify_tool(&call.name) {
        crate::execution_policy::ToolKind::Shell => true,
        // Only `job action=start` carries a command; status/tail/cancel
        // approvals must not record a shell prefix.
        crate::execution_policy::ToolKind::Job => {
            call.arguments
                .get("action")
                .and_then(|value| value.as_str())
                == Some("start")
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
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.first().is_some_and(|token| token.contains('=')) {
        return None; // leading `FOO=bar` env assignment, not a plain program
    }
    let canonical = command_shape::identity(&tokens);
    (!canonical.is_empty()).then_some(canonical)
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

    /// Whether a gated call may run without asking. Two independent layers:
    /// (1) standing consent — a configured `auto_allow` prefix or a session
    /// "a" — is mode-independent; (2) the session [`PermissionMode`] relaxes
    /// the gate more broadly. Policy hard-denials are unaffected either way:
    /// they short-circuit in the registry before any decision is consulted, so
    /// even `Yolo` never runs a denied command.
    async fn auto_approval_granted(
        &self,
        call: &ToolCall,
        request: &ApprovalRequest,
        cancel: &CancellationToken,
    ) -> bool {
        // Layer 1: standing consent (config auto_allow + session memory).
        if self
            .config
            .approval_auto_allow
            .iter()
            .any(|prefix| !prefix.is_empty() && call.name.starts_with(prefix))
        {
            return true;
        }
        let user_task = {
            let state = self.state.lock().await;
            if state.session_approved.contains(&call.name) {
                return true;
            }
            // Shell isn't blanket session-approvable by name; trust at command
            // granularity instead ("a" remembered `cargo`, `git`, …).
            if let Some(prefix) = session_shell_prefix(call)
                && state.session_trusted_shell_prefixes.contains(&prefix)
            {
                return true;
            }
            state.current_prompt.clone().unwrap_or_default()
        }; // release the state lock before any mode logic (Auto awaits a judge)

        // Layer 2: session permission mode.
        match self.permission_mode() {
            PermissionMode::Default => false,
            PermissionMode::AcceptEdits => accept_edits_approvable(&call.name, &call.arguments),
            PermissionMode::Auto => {
                self.auto_mode_approves(call, request, &user_task, cancel)
                    .await
            }
            PermissionMode::Yolo => true,
        }
    }

    /// Auto mode: a Flash classifier judges the call. Three hard floors below
    /// the judge: the top risk tier always asks (the judge can't wave it
    /// through), the offline echo backend can't judge, and a cancel mid-flight
    /// aborts into "ask". Everything else the classifier decides, failing safe
    /// to a prompt. The judge's token usage is billed to the session.
    async fn auto_mode_approves(
        &self,
        call: &ToolCall,
        request: &ApprovalRequest,
        user_task: &str,
        cancel: &CancellationToken,
    ) -> bool {
        if request.risk_level == RiskLevel::High {
            return false;
        }
        if self.client.provider_name() == crate::echo_client::EchoClient::PROVIDER {
            return false;
        }
        let action = crate::approval_classifier::action_summary(&call.arguments);
        let input = crate::approval_classifier::ClassifierInput {
            tool_name: &call.name,
            action: &action,
            risk_level: request.risk_level,
            safety_notes: &request.safety_notes,
            user_task,
        };
        let model = self.classifier_model();
        // Race the judge against cancellation so Esc during a slow classifier
        // reply aborts the call into "ask" instead of blocking the turn.
        let (approved, usage) = tokio::select! {
            biased;
            () = cancel.cancelled() => return false,
            verdict = crate::approval_classifier::approves(&*self.client, &model, &input) => verdict,
        };
        if let Some(usage) = usage {
            self.record_classifier_cost(&model, &usage).await;
        }
        approved
    }

    /// The model the auto-mode classifier runs on (see [`classifier_model_for`]).
    fn classifier_model(&self) -> String {
        classifier_model_for(&self.config, &self.registry)
    }

    /// Fold a classifier call's token cost into the running session total. The
    /// judge runs on a separate (cheap) model from the turn, so its usage never
    /// flows through the turn telemetry; without this the session cost silently
    /// under-counts every auto-mode gated call.
    async fn record_classifier_cost(&self, model: &str, usage: &crate::model::Usage) {
        let cost = crate::pricing::calculate_turn_cost(model, usage);
        let mut state = self.state.lock().await;
        state.session_cost.usd += cost.usd;
        state.session_cost.cny += cost.cny;
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
                    request.preview = self.workspace.as_deref().and_then(|ws| {
                        crate::approval_preview::build_approval_preview(&call, ws, self.ui_lang())
                    });
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
            Ok(ToolRunOutcome::ApprovalRequired { mut request }) => {
                request.preview = self.workspace.as_deref().and_then(|ws| {
                    crate::approval_preview::build_approval_preview(&current, ws, self.ui_lang())
                });
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
            if let Some(turn) = state.current_turn.as_mut() {
                turn.tool_results.push(result.clone());
            }
            // Cascade signal: a genuine execution failure means the model
            // fumbled this tool call. Denials carry their own status and user
            // cancellations carry a known marker, so neither counts. Sub-agent
            // failures are excluded too — a child that timed out, was cancelled,
            // or hit the concurrency cap is not the parent model fumbling a
            // primitive, and must not latch a session-wide Pro escalation.
            // Enough real fumbles in one turn latch escalation onto Pro (sticky
            // for the session); the latch is read by the next turn's router.
            if result.status == ToolResultStatus::Error
                && result.content != CANCELLED_TOOL_RESULT
                && !crate::subagent::is_subagent_tool(&call.name)
            {
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

/// The model the auto-mode classifier runs on. Flash is the cheap judge tier on
/// DeepSeek; a concrete non-DeepSeek model is judged on itself so auto mode
/// works off DeepSeek too. `auto`/unset are DeepSeek-only routing sentinels →
/// Flash; on a non-DeepSeek endpoint that combination is a misconfiguration that
/// already breaks normal turns, so the judge failing safe to "ask" there is
/// acceptable — and it never leaks the raw sentinel to the API layer.
fn classifier_model_for(
    config: &crate::config::AgentConfig,
    registry: &crate::model_registry::ModelRegistry,
) -> String {
    let configured = config.model.trim();
    if configured.is_empty()
        || configured.eq_ignore_ascii_case(AUTO_MODEL)
        || registry.info_for(configured).is_some()
    {
        return DEEPSEEK_V4_FLASH.to_string();
    }
    configured.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifier_model_maps_to_flash_or_concrete_model() {
        use crate::config::AgentConfig;
        use crate::model_registry::ModelRegistry;
        let reg = ModelRegistry::default();
        let cfg = |model: &str| AgentConfig {
            model: model.to_string(),
            ..AgentConfig::builtin()
        };
        // DeepSeek sentinels and catalog models → the cheap Flash judge.
        assert_eq!(classifier_model_for(&cfg("auto"), &reg), DEEPSEEK_V4_FLASH);
        assert_eq!(classifier_model_for(&cfg(""), &reg), DEEPSEEK_V4_FLASH);
        assert_eq!(
            classifier_model_for(&cfg("deepseek-v4-pro"), &reg),
            DEEPSEEK_V4_FLASH
        );
        // A concrete non-DeepSeek model is judged on itself — the raw "auto"
        // sentinel is never leaked to the API layer.
        assert_eq!(classifier_model_for(&cfg("gpt-4o"), &reg), "gpt-4o");
    }

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
            Some("cargo test".to_string())
        );
        let cancel = ToolCall {
            id: "c1".to_string(),
            name: "job".to_string(),
            arguments: json!({ "action": "cancel", "job_id": "job_1" }),
        };
        assert_eq!(session_shell_prefix(&cancel), None);
    }

    #[test]
    fn shell_prefix_is_arity_classified_not_just_leading_program() {
        // Flags on the same subcommand collapse to the same key.
        assert_eq!(
            session_shell_prefix(&shell("cargo test --all")),
            Some("cargo test".to_string())
        );
        assert_eq!(
            session_shell_prefix(&shell("  Git Status --porcelain  ")),
            Some("git status".to_string())
        );
        // A bare program with no known subcommand falls back to the program.
        assert_eq!(
            session_shell_prefix(&shell("ls -la")),
            Some("ls".to_string())
        );
    }

    #[test]
    fn session_allow_of_one_subcommand_does_not_cover_a_sibling() {
        // Regression (exfil vector): approving `git status` for the session must
        // not silently auto-approve `git push`. Distinct arity keys ⇒ the push
        // still prompts.
        let allowed = session_shell_prefix(&shell("git status")).unwrap();
        let pushed = session_shell_prefix(&shell("git push origin main")).unwrap();
        assert_eq!(allowed, "git status");
        assert_eq!(pushed, "git push");
        assert_ne!(allowed, pushed);
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
