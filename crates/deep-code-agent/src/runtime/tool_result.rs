use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::content::{Content, ContentBlock};
use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::runtime::AgentRuntime;
use crate::runtime::approval_flow::RootGrantPrompt;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::failure_class::record_failure_signals;
use crate::runtime::state::PendingToolBatch;
use crate::session_entry::Ask;
use crate::tool::{
    ApprovalDecision, RunAuthority, ToolCall, ToolCx, ToolError, ToolResult, ToolRunOutcome,
};

/// Under `Yolo`, sandboxed commands get ambient egress (unless config says
/// `never`, which stays absolute).
///
/// The per-call network gate exists so a HUMAN sees egress requests before
/// they run. Yolo removes the human from the loop, and with it everything the
/// gate protected: a hostile call would just declare `network: true` and be
/// auto-approved like anything else, so keeping the gate under yolo stops no
/// attacker — it only strands honest commands that forgot to declare
/// (`git push`, `npm install`, a trusted `cargo build` fetching crates)
/// offline, failing with raw connection errors nobody is around to interpret.
///
/// Sited in `run_tool` rather than the policy engine so it reads the LIVE
/// mode per call (Shift+Tab mid-session applies immediately, in both
/// directions) and covers every execution path — direct runs and the
/// approved re-run — in one place.
///
/// `is_subagent` is why this cannot be a pure function of the mode. Children DO
/// inherit the session's permission mode (that is what makes a dispatch usable
/// at all — see `SubAgentServices::permission_mode`), but the ambient-egress half
/// of `Yolo` must not follow it into a child dispatched WITHOUT the network
/// grant:
///
/// - that child's registry caps its network mode at `Prompt`, not `Never`
///   (`child_tool_registry`), so a `!= Never` test alone would let it through;
/// - its system prompt says, in as many words, that it has no network;
/// - and the dispatch prompt — "anything the child reads may be sent to external
///   hosts" — is the consent that grant exists to collect. Egress handed over
///   here would be a capability nobody approved, under a prompt denying it.
///
/// A granted child needs no overlay: its own policy is already `Always`.
fn yolo_ambient_network(
    plan: crate::execution_policy::ToolExecutionPlan,
    mode: crate::execution_policy::PermissionMode,
    network_mode: crate::execution_policy::NetworkMode,
    is_subagent: bool,
) -> crate::execution_policy::ToolExecutionPlan {
    let ambient_granted =
        !is_subagent || network_mode == crate::execution_policy::NetworkMode::Always;
    if mode == crate::execution_policy::PermissionMode::Yolo
        && ambient_granted
        && network_mode != crate::execution_policy::NetworkMode::Never
        && plan.requires_sandbox
    {
        return crate::execution_policy::ToolExecutionPlan {
            network: true,
            ..plan
        };
    }
    plan
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
    ///
    /// `authority` says who resolved an approval this call needed (see
    /// [`RunAuthority`]); it is read only when `decision` approves. For a call
    /// that needs no approval the registry sets `Parse` itself.
    pub(super) async fn run_tool(
        &self,
        call: &ToolCall,
        decision: Option<ApprovalDecision>,
        authority: RunAuthority,
        cancel: &CancellationToken,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<ToolRunOutcome, ToolError> {
        if !self.owns_turn(turn_id, cancel).await {
            let mut result = ToolResult::denied(call);
            result.content = CANCELLED_TOOL_RESULT.into();
            return Ok(ToolRunOutcome::Result { result });
        }
        // A sink for spend a tool incurs out-of-band (a sub-agent's own request
        // spend never reaches this turn's telemetry). Folded into the session
        // totals after the tool returns.
        let spend_sink =
            std::sync::Arc::new(std::sync::Mutex::new(crate::tool::ToolSpend::default()));
        let cx = ToolCx::new()
            .with_cancel(cancel.clone())
            .with_authority(authority)
            .with_update_fn(tool_progress_fn(tx, turn_id, call))
            .with_spend_sink(std::sync::Arc::clone(&spend_sink));
        // The network mode that will actually gate this call comes from the
        // registry's own policy, not from the config the registry was built
        // from: a child registry overrides it (capped down without a grant,
        // raised to `Always` with one), so reading `self.config` here made the
        // egress overlay — and the cache grant that rides it — disagree with the
        // gate that decides. One source of truth: the policy about to run.
        let network_mode = self.tools.policy().network_mode();
        let plan = yolo_ambient_network(
            self.tools.evaluate_tool(call),
            self.permission_mode(),
            network_mode,
            self.is_subagent,
        );
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
            // Turn totals too, not only session: a sub-agent's spend is incurred
            // during THIS turn, so the turn's `cost` (what headless `-p` and the
            // CI bot print, and `/status` shows) must include it. Folding only
            // into the session left the per-turn number low by the child's whole
            // spend on any turn that dispatched an `agent`.
            if state.current_turn_id.as_ref() == Some(turn_id) {
                state.turn_cost.usd += reported.cost.usd;
                state.turn_cost.cny += reported.cost.cny;
                state.turn_cache_hit_tokens += reported.cache_hit_tokens;
                state.turn_cache_miss_tokens += reported.cache_miss_tokens;
            }
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

    /// Whether a call may run concurrently inside a tool batch. Two conditions,
    /// both required. Structurally, only sub-agent calls qualify: each spawns
    /// an isolated child runtime, so a batch of them shares no mutable state.
    /// And the policy must let the dispatch run outright: the concurrent path
    /// has no approval gate — it cannot park the batch on a human-in-the-loop
    /// pause — so a gated dispatch (a writing role, whose spawn is the write
    /// authorization, or `network: true` under `[sandbox] network = "prompt"`)
    /// leaves the group and takes the serial path, where standing consent, the
    /// mode, the judge and the human all live. Read-only fan-out stays
    /// concurrent. Before this looked at the plan, a run of two gated
    /// dispatches failed as an error in every mode, Yolo included, while the
    /// same dispatch issued alone ran or prompted normally.
    fn is_parallel_safe(&self, call: &ToolCall) -> bool {
        crate::execution_policy::ExecPolicy::classify_tool(&call.name)
            == crate::execution_policy::ToolKind::SubAgent
            && matches!(
                self.tools.evaluate_tool(call).verdict,
                crate::execution_policy::PolicyVerdict::Allow
            )
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
            if !self.owns_turn(turn_id, cancel).await {
                remaining.push_front(call);
                self.finish_cancelled_calls(remaining, turn_id, tx).await;
                return BatchOutcome::Cancelled;
            }
            // Batch-internal parallelism: a run of independent, approval-free
            // `agent` calls issued in one turn executes concurrently — this is
            // what makes "issue several agent calls to run children in
            // parallel" true. Results are still recorded in issue order so the
            // wire transcript (and prefix cache) stays deterministic. Only
            // sub-agent calls whose plan is an outright Allow qualify (see
            // `is_parallel_safe`): they share no mutable state and never park
            // on approval, so concurrency can't race the approval machinery.
            if self.is_parallel_safe(&call)
                && remaining
                    .front()
                    .is_some_and(|next| self.is_parallel_safe(next))
            {
                let mut group = vec![call];
                while remaining
                    .front()
                    .is_some_and(|next| self.is_parallel_safe(next))
                {
                    group.push(remaining.pop_front().expect("front just checked"));
                }
                let outcomes = futures_util::future::join_all(group.iter().map(|call| {
                    self.run_tool(call, None, RunAuthority::Approved, cancel, turn_id, tx)
                }))
                .await;
                for (call, outcome) in group.iter().zip(outcomes) {
                    let result = match outcome {
                        Ok(ToolRunOutcome::Result { result }) => result,
                        // Unreachable by construction — the plan said Allow and
                        // the registry only asks when the plan or the tool's
                        // spec says so, which no sub-agent spec does. Kept as a
                        // fail-closed error rather than a panic on model input.
                        Ok(ToolRunOutcome::ApprovalRequired { .. }) => ToolResult::error(
                            call,
                            "parallel sub-agent unexpectedly requested approval",
                        ),
                        Err(error) => ToolResult::error(call, error.to_string()),
                    };
                    // Outright-Allow sub-agent plans: they never reach the gate
                    // (see `is_parallel_safe`), so nobody is asked.
                    self.record_tool_result(call, result, Ask::Unasked, tx, turn_id.clone())
                        .await;
                }
                continue;
            }
            // No decision yet: the registry runs an `Allow` on its own parse
            // authority, or asks — the authority passed here is never read.
            match self
                .run_tool(&call, None, RunAuthority::Approved, cancel, turn_id, tx)
                .await
            {
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, Ask::Unasked, tx, turn_id.clone())
                        .await;
                }
                Ok(ToolRunOutcome::ApprovalRequired { mut request }) => {
                    if let Some(granted) = self.auto_approval_granted(&call, &request, cancel).await
                    {
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
                        // The layer that resolved the prompt decides whether a
                        // shell may read the command (`AutoApproval::authority`).
                        let result = match self
                            .run_tool(
                                &call,
                                Some(ApprovalDecision::Approved),
                                granted.authority(),
                                cancel,
                                turn_id,
                                tx,
                            )
                            .await
                        {
                            Ok(ToolRunOutcome::Result { result }) => result,
                            Ok(ToolRunOutcome::ApprovalRequired { .. }) => ToolResult::error(
                                &call,
                                "tool re-requested approval after auto-approve",
                            ),
                            Err(error) => ToolResult::error(&call, error.to_string()),
                        };
                        // A standing consent resolved the gate WITHOUT putting
                        // it in front of anyone: the `ApprovalResolved` above is
                        // the only event a UI sees (no `ApprovalRequired`), so
                        // the live view leaves this call quiet — no badge, folded
                        // with the rest — and the record says the same.
                        self.record_tool_result(&call, result, Ask::Unasked, tx, turn_id.clone())
                            .await;
                        continue;
                    }
                    // A root grant is triaged before the human is involved:
                    // resolve the requested path ONCE, show that canonical
                    // form on the panel and park it with the batch (the grant
                    // must land on it, see `apply_root_grant`), and bounce an
                    // unresolvable or refused request straight back to the
                    // model — the human is never prompted for a grant that
                    // cannot actually be performed.
                    let root_grant_target = match self.root_grant_prompt_target(&call) {
                        RootGrantPrompt::NotRootGrant => None,
                        RootGrantPrompt::Resolved(canonical) => Some(canonical),
                        RootGrantPrompt::Refused(reason) => {
                            let result = ToolResult::error(&call, reason);
                            // Bounced before the human was involved: the gate
                            // was raised, but nobody was ever shown it.
                            self.record_tool_result(
                                &call,
                                result,
                                Ask::Unasked,
                                tx,
                                turn_id.clone(),
                            )
                            .await;
                            continue;
                        }
                    };
                    request.resolved_target = root_grant_target
                        .as_ref()
                        .map(|canonical| canonical.display().to_string());
                    // File I/O for a bounded diff at a human-in-the-loop
                    // pause: cheap relative to the wait, so no spawn_blocking.
                    request.preview = self.approval_preview(&call);
                    // The loop checks the token only at call boundaries, and the
                    // judge (or the root-grant resolution) awaited across this
                    // one. A cancel that landed meanwhile must end the turn
                    // here: parked, the prompt could only ever be answered into
                    // "cancelled" (`resolve_pending_tool` checks the token
                    // first) and no `TurnCancelled` would reach anyone until
                    // then. The check happens under the state lock, which is
                    // where `cancel_turn_inner` flips the token and takes
                    // `pending`: so the cancel either sees this batch parked or
                    // this park sees the cancel — the lock serializes the two.
                    let batch = PendingToolBatch {
                        approval_id: request.approval_id.clone(),
                        current: call,
                        remaining,
                        turn_id: turn_id.clone(),
                        cancel: cancel.clone(),
                        root_grant_target,
                    };
                    let unparked = {
                        let mut state = self.state.lock().await;
                        if cancel.is_cancelled() || state.current_turn_id.as_ref() != Some(turn_id)
                        {
                            Some(batch)
                        } else {
                            state.pending = Some(batch);
                            None
                        }
                    };
                    if let Some(PendingToolBatch {
                        current,
                        mut remaining,
                        ..
                    }) = unparked
                    {
                        remaining.push_front(current);
                        self.finish_cancelled_calls(remaining, turn_id, tx).await;
                        return BatchOutcome::Cancelled;
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
                    self.record_tool_result(&call, result, Ask::Unasked, tx, turn_id.clone())
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
            ..
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
            // The wait was resolved by the cancel, not by an answer — so no
            // badge, which is more accurate than the stale pending one the
            // live view keeps for these.
            self.record_tool_result(&call, result, Ask::Unasked, tx, turn_id.clone())
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

    /// `ask` is whether the approval gate put this call in front of a human,
    /// carried into the session record so a resumed transcript badges and folds
    /// exactly as the live one did. See `ExchangeResult::asked`. **Every** record
    /// from this file passes `Ask::Unasked`: none of them is reached through a
    /// parked request — that path lives in `approval_flow`, which is the only
    /// caller that has seen a human.
    pub(super) async fn record_tool_result(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
        ask: Ask,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        turn_id: TurnId,
    ) {
        self.attach_edit_diagnostics(call, &mut result, tx).await;
        let content = result.content.clone();
        let details = result.details.clone();
        let workspace = self.workspace.clone();
        match crate::tool::run_blocking("tool_output", move || {
            Ok((
                bound_tool_content(&content, workspace.as_deref()),
                bound_tool_details(details, workspace.as_deref()),
            ))
        })
        .await
        {
            Ok((content, details)) => {
                result.content = content;
                result.details = details;
            }
            Err(error) => {
                result
                    .content
                    .push_text(format!("\n[output preparation failed: {error}]"));
                result.content = bound_tool_content(&result.content, None);
                result.details = bound_tool_details(result.details.take(), None);
            }
        }

        {
            let mut state = self.state.lock().await;
            if !state.session.record_tool_result(
                &result.call_id,
                result.content.clone(),
                result.status,
                ask,
                result.details.clone(),
            ) {
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
            if state.current_turn_id.as_ref() == Some(&turn_id) {
                record_failure_signals(&mut state, call, &result);
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

/// Maximum text characters projected into one tool observation. Image blocks
/// remain ordered; omitted text gets a bounded, readable spill source.
pub(crate) const TOOL_OUTPUT_BUDGET: usize = 12_000;

/// Serialized UI details per result, including room for their indentation in
/// the session record. Model text and UI metadata have independent budgets.
const TOOL_DETAILS_BUDGET_BYTES: usize = 16 * 1024;

fn details_storage_size(details: &serde_json::Value) -> usize {
    let json = serde_json::to_vec_pretty(details).expect("JSON values serialize");
    // A details value sits inside entries/exchanges/result. Allow 32 extra
    // spaces per structural newline, beyond the actual session indentation.
    json.len().saturating_add(
        json.iter()
            .filter(|byte| **byte == b'\n')
            .count()
            .saturating_mul(32),
    )
}

fn bound_tool_details(
    details: Option<serde_json::Value>,
    workspace: Option<&std::path::Path>,
) -> Option<serde_json::Value> {
    let mut preview = details?;
    if details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES {
        return Some(preview);
    }
    let original = serde_json::to_string(&preview).expect("JSON values serialize");
    let (path, complete, error) = match workspace.map(|root| spill_tool_text(root, &original)) {
        Some(Ok((path, complete))) => (Some(path), complete, None),
        Some(Err(error)) => (None, false, Some(error.to_string())),
        None => (
            None,
            false,
            Some("no output workspace is available".to_string()),
        ),
    };
    let storage = serde_json::json!({
        "inline_truncated": true,
        "path": path,
        "complete": complete,
        "original_bytes": original.len(),
        "error": error.map(|error| error.chars().take(256).collect::<String>()),
    });
    let Some(object) = preview.as_object_mut() else {
        return Some(serde_json::json!({"details_storage": storage}));
    };
    object.insert("details_storage".into(), storage.clone());

    // Shorten display payloads without modifying identities or execution
    // facts such as capture_complete, byte counters and process exit status.
    let mut paths = vec![
        "/edit/diff".to_string(),
        "/stdout/text".into(),
        "/stderr/text".into(),
    ];
    if let Some(todos) = preview.get("todos").and_then(serde_json::Value::as_array) {
        paths.extend((0..todos.len()).map(|index| format!("/todos/{index}/content")));
    }
    let sources: Vec<_> = paths
        .into_iter()
        .filter_map(|path| {
            let serde_json::Value::String(text) = preview.pointer_mut(&path)? else {
                return None;
            };
            Some((path, std::mem::take(text)))
        })
        .collect();
    if sources.is_empty() {
        return Some(serde_json::json!({"details_storage": storage}));
    }
    for limit in [2048, 1024, 512, 256, 128, 64] {
        for (path, source) in &sources {
            let text = if source.chars().count() <= limit {
                source.clone()
            } else {
                crate::text_util::clip_middle(source, limit, limit / 3, limit / 3, |_| {
                    "\n...[details preview omitted]...\n".into()
                })
            };
            *preview
                .pointer_mut(path)
                .expect("preview path remains present") = text.into();
            if path == "/edit/diff" && source.chars().count() > limit {
                preview["edit"]["truncated"] = true.into();
            }
        }
        if details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES {
            return Some(preview);
        }
    }
    // Unknown/large schemas still have a recoverable source, but cannot be
    // allowed to exhaust the transcript's storage budget.
    Some(serde_json::json!({"details_storage": storage}))
}

/// Bound a tool result before it re-enters the model context: oversized
/// outputs keep their head and tail with a marker for the elided middle, so
/// the next request stays small without losing the most relevant ends.
/// Counts by `char` to stay UTF-8 safe.
fn bound_tool_content(content: &Content, workspace: Option<&std::path::Path>) -> Content {
    let text_chars = content
        .text_blocks()
        .map(|text| text.chars().count())
        .sum::<usize>();
    if text_chars <= TOOL_OUTPUT_BUDGET {
        return content.clone();
    }
    let full_text = content.summary_text();
    let continuation = match workspace.map(|root| spill_tool_text(root, &full_text)) {
        Some(Ok((path, complete))) => format!("\n[Inline output shortened. {} saved to {}; use read_file with next_start_byte for bounded continuation.]\n", if complete { "Full output" } else { "Only the first 64 MiB" }, path.display()),
        Some(Err(error)) => format!("\n[Inline output shortened; full output could not be saved: {error}]\n"),
        None => "\n[Inline output shortened; this runtime has no output workspace, so the omitted text is unavailable.]\n".into(),
    };
    let mut remaining = TOOL_OUTPUT_BUDGET.saturating_sub(continuation.chars().count());
    let mut text_left = content.text_blocks().count();
    let mut bounded = Content::default();
    for block in &content.blocks {
        match block {
            ContentBlock::Text { text } => {
                let share = remaining / text_left.max(1);
                let clipped = if text.chars().count() <= share {
                    text.clone()
                } else if share < 64 {
                    text.chars().take(share).collect()
                } else {
                    crate::text_util::clip_middle(text, share, share / 3, share / 3, |count| {
                        format!("\n...[{count} characters omitted]...\n")
                    })
                };
                remaining = remaining.saturating_sub(clipped.chars().count());
                text_left = text_left.saturating_sub(1);
                if !clipped.is_empty() {
                    bounded.push_text(clipped);
                }
            }
            ContentBlock::Image { image } => bounded.push_image(image.clone()),
        }
    }
    bounded.push_text(continuation);
    bounded
}

fn spill_tool_text(
    root: &std::path::Path,
    text: &str,
) -> std::io::Result<(std::path::PathBuf, bool)> {
    use std::io::Write;
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    let directory = crate::pinned_dir::open_canonical(root)?;
    let state = crate::pinned_dir::owned_child(&directory, ".deep-code")?;
    let spill = crate::pinned_dir::owned_child(&state, "spill")?;
    let results = crate::pinned_dir::owned_child(&spill, "tool-results")?;
    let filename = format!("{}.txt", uuid::Uuid::new_v4());
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = results.open_with(&filename, &options)?;
    let mut end = text.len().min(MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    file.write_all(&text.as_bytes()[..end])?;
    file.sync_data()?;
    Ok((
        std::path::PathBuf::from(".deep-code/spill/tool-results").join(filename),
        end == text.len(),
    ))
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
        kind: crate::RuntimeFailureKind::Execution,
        turn_id,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::{ExecPolicy, NetworkMode, PermissionMode};
    use serde_json::json;

    #[test]
    fn small_details_are_preserved_without_a_storage_reference() {
        let details = json!({"job_id": "job", "status": "completed", "exit_code": 0});
        assert_eq!(
            bound_tool_details(Some(details.clone()), None),
            Some(details)
        );
        assert_eq!(bound_tool_details(None, None), None);
    }

    #[test]
    fn details_budget_counts_utf8_and_json_escaping_and_preserves_edit_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let original = json!({"edit": {
            "path": "unicode.txt", "diff": "界\t\"\u{1}".repeat(20_000),
            "added": 1, "removed": 1, "truncated": false, "new_file": false,
        }});
        let preview = bound_tool_details(Some(original.clone()), Some(&root)).unwrap();
        assert!(details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES);
        assert_eq!(preview["edit"]["path"], "unicode.txt");
        assert_eq!(preview["edit"]["added"], 1);
        assert_eq!(preview["edit"]["removed"], 1);
        assert_eq!(preview["edit"]["new_file"], false);
        assert_eq!(preview["edit"]["truncated"], true);
        let source = &preview["details_storage"];
        assert_eq!(source["complete"], true);
        let saved = std::fs::read(root.join(source["path"].as_str().unwrap())).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&saved).unwrap(),
            original
        );
        assert_eq!(source["original_bytes"], saved.len());
    }

    #[test]
    fn details_preview_does_not_change_execution_capture_facts() {
        let text = "😀".repeat(20_000);
        let original = json!({
            "job_id": "job", "status": "completed", "exit_code": 0,
            "stdout": {"text": text, "capture_complete": true, "finished": true,
                "inline_truncated": false, "total_bytes": text.len(), "read_error": null},
            "stderr": {"text": "", "capture_complete": true, "finished": true},
        });
        let preview = bound_tool_details(Some(original.clone()), None).unwrap();
        assert!(details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES);
        assert!(preview["stdout"]["text"].as_str().unwrap().len() < text.len());
        for key in [
            "capture_complete",
            "finished",
            "inline_truncated",
            "total_bytes",
            "read_error",
        ] {
            assert_eq!(preview["stdout"][key], original["stdout"][key]);
        }
        assert_eq!(preview["details_storage"]["complete"], false);
        assert!(preview["details_storage"]["path"].is_null());
        assert!(
            preview["details_storage"]["error"]
                .as_str()
                .unwrap()
                .contains("workspace")
        );
    }

    #[test]
    fn details_budget_accounts_for_nested_pretty_whitespace_and_unknown_schemas() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let original = json!({"rows": vec![json!({}); 1000]});
        assert!(serde_json::to_vec(&original).unwrap().len() < TOOL_DETAILS_BUDGET_BYTES);
        assert!(details_storage_size(&original) > TOOL_DETAILS_BUDGET_BYTES);
        let preview = bound_tool_details(Some(original.clone()), Some(&root)).unwrap();
        assert!(details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES);
        let source = root.join(preview["details_storage"]["path"].as_str().unwrap());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(source).unwrap()).unwrap(),
            original
        );
        assert!(preview.get("rows").is_none());
    }

    #[test]
    fn details_remain_bounded_and_report_a_failed_sidecar_write() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join(".deep-code"), "not a directory").unwrap();
        let preview =
            bound_tool_details(Some(json!({"payload": "x".repeat(100_000)})), Some(&root)).unwrap();
        assert!(details_storage_size(&preview) <= TOOL_DETAILS_BUDGET_BYTES);
        assert_eq!(preview["details_storage"]["complete"], false);
        assert!(preview["details_storage"]["path"].is_null());
        assert!(preview["details_storage"]["error"].is_string());
    }

    #[test]
    fn bounded_content_preserves_images_and_exposes_the_complete_source() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let image = crate::image::snapshot_bytes(
            &workspace,
            include_bytes!("../image/tests/fixtures/red.png"),
        )
        .unwrap();
        let content = Content::new(vec![
            ContentBlock::Text {
                text: "前".repeat(20_000),
            },
            ContentBlock::Image {
                image: image.clone(),
            },
            ContentBlock::Text {
                text: "後".repeat(20_000),
            },
        ]);
        let bounded = bound_tool_content(&content, Some(&workspace));
        assert!(bounded.text().chars().count() <= TOOL_OUTPUT_BUDGET);
        assert!(
            matches!(&bounded.blocks[1], ContentBlock::Image { image: actual } if actual == &image)
        );
        assert!(
            bounded
                .text()
                .contains("Full output saved to .deep-code/spill/tool-results/")
        );
        let files = std::fs::read_dir(workspace.join(".deep-code/spill/tool-results"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            std::fs::read_to_string(files[0].path()).unwrap(),
            content.summary_text()
        );
    }

    #[cfg(unix)]
    #[test]
    fn tool_spills_are_private_inside_an_existing_directory() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let directory = root.join(".deep-code/spill/tool-results");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (path, complete) = spill_tool_text(&root, "private tool data").unwrap();
        assert!(complete);
        assert_eq!(
            std::fs::metadata(root.join(path)).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn many_small_blocks_stay_inside_one_output_budget() {
        let content: Content = Content::new(
            (0..20_000)
                .map(|_| ContentBlock::Text {
                    text: "文字".into(),
                })
                .collect(),
        );
        let bounded = bound_tool_content(&content, None);
        assert!(bounded.text().chars().count() <= TOOL_OUTPUT_BUDGET);
        assert!(bounded.text().contains("omitted text is unavailable"));
    }

    /// The plan an UNDECLARED command gets: this is the exact shape that used
    /// to run offline under yolo — `git push` (untrusted, prompts) and
    /// `cargo build` (trusted, silent) both carried `network: false`.
    fn undeclared_shell_plan(command: &str) -> crate::execution_policy::ToolExecutionPlan {
        ExecPolicy::default().evaluate_tool("shell", &json!({ "command": command }))
    }

    #[test]
    fn yolo_grants_ambient_egress_to_sandboxed_commands() {
        for command in ["cargo build", "git push origin main", "npm install"] {
            let plan = undeclared_shell_plan(command);
            assert!(plan.requires_sandbox && !plan.network, "precondition");
            let yolo = yolo_ambient_network(plan, PermissionMode::Yolo, NetworkMode::Prompt, false);
            assert!(yolo.network, "{command} must get egress under yolo");
        }
        // A declared command keeps its grant — the override is idempotent.
        let declared = ExecPolicy::default()
            .evaluate_tool("shell", &json!({"command": "git push", "network": true}));
        assert!(declared.network, "precondition");
        assert!(
            yolo_ambient_network(declared, PermissionMode::Yolo, NetworkMode::Prompt, false)
                .network
        );
    }

    /// `never` is the user's absolute refusal and yolo must not override it;
    /// non-yolo modes keep the human-gated behavior untouched; and plans that
    /// never enter the sandbox (in-process tools) gain nothing — Unsandboxed
    /// execution is not a network grant.
    #[test]
    fn ambient_egress_stops_at_never_other_modes_and_unsandboxed_plans() {
        let plan = undeclared_shell_plan("git push origin main");
        assert!(
            !yolo_ambient_network(
                plan.clone(),
                PermissionMode::Yolo,
                NetworkMode::Never,
                false
            )
            .network,
            "never stays absolute"
        );
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
        ] {
            assert!(
                !yolo_ambient_network(plan.clone(), mode, NetworkMode::Prompt, false).network,
                "{mode:?} must keep the human-gated behavior"
            );
        }
        let write = ExecPolicy::default()
            .evaluate_tool("write_file", &json!({"path": "a.rs", "content": "x"}));
        assert!(!write.requires_sandbox, "precondition");
        assert!(
            !yolo_ambient_network(write, PermissionMode::Yolo, NetworkMode::Prompt, false).network
        );
    }

    /// A child dispatched WITHOUT the network grant must not ride the parent's
    /// yolo egress. Its registry caps the mode at `Prompt` rather than `Never`
    /// (that is what makes the cap a cap: a declared command there would ask a
    /// human who is not there), so the `!= Never` test alone let ambient egress
    /// through — into a child whose system prompt said it had none, and whose
    /// dispatch never collected the "anything it reads may be sent to external
    /// hosts" consent.
    #[test]
    fn yolo_egress_does_not_reach_a_child_without_the_dispatch_grant() {
        let plan = undeclared_shell_plan("npm install");

        // The parent: unchanged.
        assert!(
            yolo_ambient_network(
                plan.clone(),
                PermissionMode::Yolo,
                NetworkMode::Prompt,
                false
            )
            .network
        );
        // The ungranted child: capped at `Prompt`, and left offline.
        assert!(
            !yolo_ambient_network(
                plan.clone(),
                PermissionMode::Yolo,
                NetworkMode::Prompt,
                true
            )
            .network,
            "an ungranted child's commands stay offline under yolo"
        );
        // A granted child runs on `Always` and needs no overlay — it already has
        // ambient egress, so the overlay is a no-op rather than a second grant.
        assert!(
            yolo_ambient_network(
                plan.clone(),
                PermissionMode::Yolo,
                NetworkMode::Always,
                true
            )
            .network
        );
        // And the `never` floor still outranks every combination.
        assert!(
            !yolo_ambient_network(plan, PermissionMode::Yolo, NetworkMode::Never, true).network
        );
    }
}
