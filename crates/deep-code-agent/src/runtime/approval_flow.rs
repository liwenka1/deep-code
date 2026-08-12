//! The approval gate's resolution layer: standing consents (config
//! `auto_allow` + session "a"), the session permission mode, the auto-mode
//! Flash judge, and the application of the user's explicit decision. The
//! batch machinery in `tool_result.rs` consults this before parking a call.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{PermissionMode, RiskLevel, accept_edits_approvable, command_shape};
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, emit};
use crate::runtime::state::PendingToolBatch;
use crate::runtime::tool_result::BatchOutcome;
use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolResult, ToolRunOutcome};

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

impl AgentRuntime {
    /// Whether a gated call may run without asking. Two independent layers:
    /// (1) standing consent — a configured `auto_allow` prefix or a session
    /// "a" — is mode-independent; (2) the session [`PermissionMode`] relaxes
    /// the gate more broadly. Policy hard-denials are unaffected either way:
    /// they short-circuit in the registry before any decision is consulted.
    /// (That covers commands the deny parser recognized — it is best-effort,
    /// so `Yolo`'s real containment is the OS sandbox, not the deny list.)
    pub(super) async fn auto_approval_granted(
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
                // A network declaration sits above the judge: opening egress
                // is the human's call (or `[sandbox] network = "always"`),
                // never something a classifier waves through. Short-circuits
                // before the judge spends a request.
                !request.network
                    && self
                        .auto_mode_approves(call, request, &user_task, cancel)
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
        // Auto is at least as permissive as AcceptEdits (it sits above it in the
        // mode cycle), so inherit its bounded fs-edit allowances first. Without
        // this, a "more permissive" mode would ask for a plain `mkdir src/x`
        // that the stricter AcceptEdits waves through — shell defaults to the
        // High risk tier, which the judge floor below always prompts on.
        if accept_edits_approvable(&call.name, &call.arguments) {
            return true;
        }
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
        let cache_hit = usage.prompt_cache_hit_tokens.unwrap_or(0);
        let cache_miss = usage.prompt_cache_miss_tokens.unwrap_or(0);
        let savings = crate::pricing::cache_savings(model, cache_hit);
        let mut state = self.state.lock().await;
        state.session_cost.usd += cost.usd;
        state.session_cost.cny += cost.cny;
        // Fold cache tokens too, so the session cache-hit rate/savings read
        // consistently with `accumulate_request_usage`.
        state.session_cache_hit_tokens += u64::from(cache_hit);
        state.session_cache_miss_tokens += u64::from(cache_miss);
        state.session_cache_savings.usd += savings.usd;
        state.session_cache_savings.cny += savings.cny;
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
                    let roots =
                        crate::workspace_policy::WorkspaceRoots::new(ws, self.extra_roots.clone());
                    crate::approval_preview::build_approval_preview(
                        &current,
                        &roots,
                        self.ui_lang(),
                    )
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
}

/// The model the auto-mode classifier runs on. Flash is the cheap judge tier;
/// `auto`/unset routing sentinels and any catalog model resolve to it. A model
/// string the catalog does not know — a passthrough id, e.g. a newer DeepSeek
/// model the registry hasn't caught up to — is judged on itself rather than on a
/// Flash that may not exist for it. This stays DeepSeek-centric: cost for an
/// off-catalog model records as zero (pricing is table-driven), so an off-DeepSeek
/// `base_url` is best-effort, not a supported multi-provider mode. Either way the
/// raw `auto` sentinel never leaks to the API layer.
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
        // A passthrough id the catalog doesn't know (e.g. a newer DeepSeek model
        // the registry hasn't caught up to) is judged on itself — and the raw
        // "auto" sentinel never leaks to the API layer.
        assert_eq!(
            classifier_model_for(&cfg("deepseek-v9-experimental"), &reg),
            "deepseek-v9-experimental"
        );
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
