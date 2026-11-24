//! Resolve approval requests through standing consent, the session permission
//! mode, or an explicit decision. The batch machinery in `tool_result.rs`
//! consults this module before parking a call.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{
    ApprovalAuthority, PermissionMode, accept_edits_approvable, command_shape, network_requested,
    shell_command_of,
};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, ToolCallId, TurnId, emit};
use crate::runtime::state::PendingToolBatch;
use crate::runtime::tool_result::BatchOutcome;
use crate::session_entry::Ask;
use crate::tool::{ApprovalDecision, RunAuthority, ToolCall, ToolResult, ToolRunOutcome};

/// Whether "approve for the whole session" may be recorded for a tool.
/// Shell-class tools are excluded: their risk lives in the per-call
/// arguments, so a blanket session consent would be misleading. The `job`
/// tool is excluded for the same reason — a session-allow granted on a
/// `cancel` prompt must not blanket-approve future `action=start` commands.
/// Sub-agent dispatch is excluded on the same grounds: what its prompt
/// authorizes is in the arguments — a writing role, or `network: true` — and
/// a consent recorded on one kind of dispatch would silently cover the other.
/// Root grants require explicit approval for the requested directory.
///
/// Network tools (`fetch_url`/`web_search`) also carry their risk in the URL or
/// query. Remembering one approval by tool name would authorize every future
/// request. [`session_consent_recordable`] uses this rule to decide when the
/// panel should offer "a".
pub(super) fn session_allowable(tool_name: &str) -> bool {
    !matches!(
        crate::execution_policy::ExecPolicy::classify_tool(tool_name),
        crate::execution_policy::ToolKind::Shell
            | crate::execution_policy::ToolKind::Job
            | crate::execution_policy::ToolKind::SubAgent
            | crate::execution_policy::ToolKind::RootGrant
            | crate::execution_policy::ToolKind::Network
    )
}

/// Whether answering "approve for the session" at this prompt would record
/// anything at all: either the tool is `session_allowable` by name, or the
/// call is one simple shell command whose identity can be remembered
/// (`shell_consent_identity`) — and in neither case is it a `HumanOnly` call.
/// `false` means "a" would silently downgrade to a one-time approve: a UI reads
/// this to offer only the options that mean what they say, and it is the same
/// rules the recording path applies, so the panel and the runtime cannot
/// disagree about what "a" does.
///
/// `HumanOnly` is the third rule and the reason this takes authority: an
/// irreversible command is refused to every automatic path ABOVE this question
/// (`auto_approval_granted` returns before consulting session memory), so a
/// recorded identity for it would never be read. Offering "a" would print an
/// option whose effect is "this once" — and, worse here than elsewhere, the
/// identity a shell consent records drops the flags, so "I approved that force
/// push for the session" would have been remembered as plain `git push`.
///
/// Takes the arguments by reference all the way down, because a UI asks this
/// on the *render* path — the approval panel decides between `y/a/n` and `y/n`
/// on every frame it draws. Building a throwaway `ToolCall` to ask made that
/// a deep clone of the call's whole JSON argument object per frame, for a
/// question that only ever reads one string out of it.
#[must_use]
pub fn session_consent_recordable(
    tool_name: &str,
    arguments: &serde_json::Value,
    authority: Option<ApprovalAuthority>,
) -> bool {
    if authority == Some(ApprovalAuthority::HumanOnly) {
        return false;
    }
    session_allowable(tool_name) || shell_consent_identity(tool_name, arguments).is_some()
}

/// The remembered identity of a command-bearing call, from the two fields that
/// decide it. The one body behind [`session_shell_prefix`] and
/// [`session_consent_recordable`], so the panel's "would `a` record anything"
/// and the runtime's "what does `a` record" cannot answer from different rules.
fn shell_consent_identity(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    if arguments.get("stdin").is_some_and(|value| !value.is_null())
        || arguments
            .get("interactive")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return None;
    }
    // Two rules, each with one home: `shell_command_of` says which calls carry
    // a command at all (`shell`, or `job action=start` — status/tail/cancel
    // must never record a prefix), and `command_shape::session_identity` says
    // what counts as one simple command, read through the same lexer as the
    // trust gate and the deny floor.
    command_shape::session_identity(shell_command_of(tool_name, arguments)?)
}

/// Whether a command-bearing call could actually run unattended (as argv, no
/// shell) on this host. Only Windows ever answers `false`: a cmd builtin
/// (`mkdir`) or a `.cmd`/`.bat` shim (`npm`, `npx`, `yarn`, `pnpm`) does not
/// resolve to a real `.exe`/`.com`, so the executor refuses to run it as argv
/// and will not route it through `cmd.exe`. Such a command must keep PROMPTING —
/// each approval runs it as text through `cmd /C`, which a human reads — instead
/// of being auto-approved as argv (a session consent, or accept-edits) and then
/// failing to spawn for the rest of the session (the `26cbaf1` regression).
///
/// Non-command calls (`write_file`, a sub-agent dispatch) and, on Unix, every
/// command answer `true`, so this is a no-op everywhere but Windows. A compound
/// command must have EVERY segment runnable, since each runs as its own argv. An
/// unparsable line answers `true` and is left to the executor, which refuses it
/// with the unattended-unparsable note rather than through this gate.
fn command_runnable_unattended(call: &ToolCall) -> bool {
    let Some(command) = shell_command_of(&call.name, &call.arguments) else {
        return true; // not command-bearing
    };
    match crate::execution_policy::parse_unattended(command) {
        Some(commands) => commands.iter().all(|parsed| {
            parsed
                .argv
                .first()
                .is_some_and(|program| crate::sandbox::program_runs_as_argv(program))
        }),
        None => true,
    }
}

/// Whether a call is the `request_write_root` doorbell (see
/// [`crate::execution_policy::ToolKind::RootGrant`]).
pub(super) fn is_root_grant(tool_name: &str) -> bool {
    crate::execution_policy::ExecPolicy::classify_tool(tool_name)
        == crate::execution_policy::ToolKind::RootGrant
}

/// Whether a tool has intrinsic network access (`fetch_url`/`web_search`).
/// The approval panel uses this alongside the call's declared `network` flag
/// to focus Deny on egress requests.
#[must_use]
pub fn is_network_tool(tool_name: &str) -> bool {
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
    shell_consent_identity(&call.name, &call.arguments)
}

/// What a session "a" on a shell call records, and what a later call must
/// present to ride it: the command identity ([`session_shell_prefix`]) together
/// with whether the call declares `network: true`. The declaration is part of
/// the consent, not a detail beside it — a consent recorded on an offline
/// `npm test` must not wave through `npm test` with egress, for the same reason
/// a consent on one kind of sub-agent dispatch does not cover the other (see
/// [`session_allowable`]). Standing consent is checked before the permission mode.
pub(super) fn shell_consent_key(call: &ToolCall) -> Option<(String, bool)> {
    let identity = session_shell_prefix(call)?;
    Some((identity, network_requested(&call.arguments)))
}

impl AgentRuntime {
    /// Resolve a gated call through standing consent or the session mode.
    /// `None` leaves the request for an explicit decision. The returned
    /// authority determines how the executor runs a command.
    pub(super) async fn auto_approval_granted(
        &self,
        call: &ToolCall,
        required_authority: ApprovalAuthority,
    ) -> Option<RunAuthority> {
        // Root grants and HumanOnly plans require a decision for this request,
        // regardless of standing consent or mode. Unattended consumers deny them.
        if is_root_grant(&call.name) || required_authority == ApprovalAuthority::HumanOnly {
            return None;
        }
        // A child's gated calls use its dispatch grants in
        // `subagent_approval_decision`. Applying parent consent here would let
        // the child exceed its granted authority.
        if self.is_subagent {
            return None;
        }
        // Layer 1: standing consent (config auto_allow + session memory).
        // Exact name match, not a prefix: standing consent must not stretch.
        // A prefix `"s"` would have covered every s-tool at once, and a tool
        // added later whose name happens to extend a consented entry would
        // have shipped pre-approved without anyone deciding that.
        if self.config.approval_auto_allow.contains(&call.name) {
            return Some(RunAuthority::Approved);
        }
        {
            let state = self.state.lock().await;
            if state.session_approved.contains(&call.name) {
                return Some(RunAuthority::Approved);
            }
            // Shell isn't blanket session-approvable by name; trust at command
            // granularity instead ("a" remembered `cargo test`, `git push`, …),
            // and only for the egress the consent was given with. A remembered
            // command must be runnable as argv. Windows shell builtins and
            // `.cmd` shims fall through to the mode below.
            if let Some(key) = shell_consent_key(call)
                && state.session_trusted_shell_prefixes.contains(&key)
                && command_runnable_unattended(call)
            {
                return Some(RunAuthority::Parse);
            }
        }

        // Layer 2: session permission mode.
        match self.permission_mode() {
            PermissionMode::Default => None,
            PermissionMode::AcceptEdits => (accept_edits_approvable(&call.name, &call.arguments)
                && command_runnable_unattended(call))
            .then_some(RunAuthority::Parse),
            PermissionMode::Yolo => Some(RunAuthority::Approved),
        }
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
        let PendingToolBatch {
            current,
            remaining,
            turn_id,
            cancel,
            root_grant_target,
            ..
        } = pending;
        // Validate ownership and record standing consent under the same lock.
        // `begin_turn` may have superseded this batch after it was submitted
        // but before this spawned task received its first poll.
        let decision = {
            let mut state = self.state.lock().await;
            if state.current_turn_id.as_ref() != Some(&turn_id) || cancel.is_cancelled() {
                drop(state);
                let mut calls = remaining;
                calls.push_front(current);
                self.finish_cancelled_calls(calls, &turn_id, tx).await;
                return;
            }
            if decision == ApprovalDecision::ApprovedForSession {
                let needs_human = self.tools.evaluate_tool(&current).requires_human();
                if !needs_human {
                    if session_allowable(&current.name) {
                        state.session_approved.insert(current.name.clone());
                    } else if let Some((identity, network)) = shell_consent_key(&current) {
                        if network {
                            state
                                .session_trusted_shell_prefixes
                                .insert((identity.clone(), false));
                        }
                        state
                            .session_trusted_shell_prefixes
                            .insert((identity, network));
                    }
                }
                ApprovalDecision::Approved
            } else {
                decision
            }
        };
        // What goes into the record. Every path that reaches this function is
        // resolving a PARKED call — one that was put in front of whoever answers
        // for this runtime, which is what the live view badges it for — so the
        // record says so whatever the answer was. The answer itself, refusals
        // above all, is carried by `status`, so it needs no flag of its own.
        //
        // Inside a sub-agent, "whoever answers" is the child's role decider, not
        // a human. That exchange lands in the child's in-memory session
        // (`with_system_prompt_shared` builds a bare `Session` — no store, no
        // id) and is never resumed, so it reaches no transcript a badge could
        // mislead in. Give a child persistence and this needs revisiting: `asked`
        // would then mark a call the human never saw.
        let ask = Ask::Asked;
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
                    result.content = denial_note
                        .clone()
                        .unwrap_or_else(|| {
                            format!(
                                "User declined the write-root request for '{}'. Do not request \
                             this path again; continue within the granted roots, or the user \
                             can grant it later with /add-dir.",
                                root_grant_requested_path(&current)
                            )
                        })
                        .into();
                    result
                }
                _ => {
                    self.apply_root_grant(
                        &current,
                        root_grant_target.as_deref(),
                        &turn_id,
                        &cancel,
                        tx,
                    )
                    .await
                }
            };
            self.record_tool_result(&current, result, ask, tx, turn_id.clone())
                .await;
            self.resume_after_batch(remaining, &turn_id, &cancel, tx)
                .await;
            return;
        }
        match self
            .run_tool(
                &current,
                Some(decision),
                RunAuthority::Approved,
                &cancel,
                &turn_id,
                tx,
            )
            .await
        {
            Ok(ToolRunOutcome::Result { mut result }) => {
                // Same substitution as the root-grant arm above: an unattended
                // denial carries its real reason instead of "denied by user".
                if result.status == crate::tool::ToolResultStatus::Denied
                    && let Some(note) = denial_note
                {
                    result.content = note.into();
                }
                self.record_tool_result(&current, result, ask, tx, turn_id.clone())
                    .await;
            }
            Ok(ToolRunOutcome::ApprovalRequired { mut request }) => {
                // Same guard as the batch's park site (`process_tool_batch`):
                // the tool re-requested approval after an await, and a cancel
                // that landed during it ends the turn instead of parking a
                // prompt nobody can answer into anything but "cancelled". The
                // check is made under the state lock, where `cancel_turn_inner`
                // flips the token and takes `pending`, so the two cannot
                // interleave into a parked batch with a cancelled token.
                request.preview = self.approval_preview(&current);
                let batch = PendingToolBatch {
                    approval_id: request.approval_id.clone(),
                    current,
                    remaining,
                    turn_id: turn_id.clone(),
                    cancel: cancel.clone(),
                    // A root grant never reaches this re-prompt path (it
                    // is intercepted above, before run_tool); None keeps
                    // the grant fail-closed if that ever changes.
                    root_grant_target: None,
                };
                let unparked = {
                    let mut state = self.state.lock().await;
                    if cancel.is_cancelled() || state.current_turn_id.as_ref() != Some(&turn_id) {
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
                    self.finish_cancelled_calls(remaining, &turn_id, tx).await;
                    return;
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
                self.record_tool_result(&current, result, ask, tx, turn_id.clone())
                    .await;
            }
        }

        // Resolved call recorded; drain the rest of the batch, then resume the
        // loop to feed all tool results into the next chat turn.
        self.resume_after_batch(remaining, &turn_id, &cancel, tx)
            .await;
    }

    /// Drain the rest of a batch a human answer resumed, then continue the
    /// turn — under the same exit the loop's own batches get: a batch that
    /// pushed the turn's boundary denials past the breaker ends the turn with
    /// guidance instead of another model round. Without this the breaker fired
    /// one round late whenever the tripping batch was the one resumed here.
    async fn resume_after_batch(
        &self,
        remaining: std::collections::VecDeque<ToolCall>,
        turn_id: &TurnId,
        cancel: &CancellationToken,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if !self.owns_turn(turn_id, cancel).await {
            self.finish_cancelled_calls(remaining, turn_id, tx).await;
            return;
        }
        if self
            .process_tool_batch(remaining, turn_id, cancel, tx)
            .await
            == BatchOutcome::Completed
            && !self.boundary_breaker_tripped(turn_id, tx).await
        {
            self.run_loop(turn_id.clone(), cancel.clone(), tx).await;
        }
    }

    /// Human-reviewable preview for a gated call, built against the live
    /// boundary so absolute paths into mid-session grants resolve too.
    pub(super) fn approval_preview(&self, call: &ToolCall) -> Option<String> {
        let boundary = self.boundary.as_ref()?;
        crate::approval_preview::build_approval_preview_with_policy(call, boundary, self.ui_lang())
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
        cancel: &CancellationToken,
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
        let state = self.state.lock().await;
        if state.current_turn_id.as_ref() != Some(turn_id) || cancel.is_cancelled() {
            let mut result = ToolResult::denied(call);
            result.content = super::tool_result::CANCELLED_TOOL_RESULT.into();
            return result;
        }
        let outcome = boundary.grant_resolved(fresh);
        drop(state);
        match outcome {
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
            let call = shell(command);
            // Two rules can answer None; pin which one did. A `shell` call is
            // always command-bearing, so the refusal must come from the
            // identity rule — a refactor that moved it between the two homes
            // (or dropped it from one) would fail here, not just the sum.
            assert!(
                call.shell_command().is_some(),
                "{command:?} is command-bearing"
            );
            assert_eq!(
                command_shape::session_identity(command),
                None,
                "the identity rule must refuse {command:?}"
            );
            assert_eq!(
                session_shell_prefix(&call),
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

    /// The consent key carries the egress declaration: an offline consent must
    /// not cover the network variant (the Auto egress floor sits below standing
    /// consent, so a flag-blind key skipped it in every mode), while the
    /// identity half still collapses flags the way `session_shell_prefix` does.
    #[test]
    fn shell_consent_key_carries_the_network_declaration() {
        let networked = |command: &str| ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            arguments: json!({ "command": command, "network": true }),
        };
        assert_eq!(
            shell_consent_key(&shell("npm test")),
            Some(("npm test".to_string(), false))
        );
        assert_eq!(
            shell_consent_key(&networked("npm test --coverage")),
            Some(("npm test".to_string(), true))
        );
        assert_ne!(
            shell_consent_key(&shell("npm test")),
            shell_consent_key(&networked("npm test"))
        );
        // `job action=start` is keyed the same way; a compound command has no
        // key at all, declared or not.
        assert_eq!(
            shell_consent_key(&job_start("git push origin main")),
            Some(("git push".to_string(), false))
        );
        assert_eq!(shell_consent_key(&networked("npm test && curl x")), None);
    }

    /// The argv-runnable gate: a command auto-approved by a session consent or
    /// accept-edits runs as argv, so a program word that cannot run that way on
    /// this host must not be auto-approved (it would fail to spawn every time).
    /// Non-command calls bypass it, and on Unix every command passes.
    #[test]
    fn command_runnable_gate_bypasses_non_command_calls() {
        let write = ToolCall {
            id: "c1".to_string(),
            name: "write_file".to_string(),
            arguments: json!({ "path": "a.rs", "content": "x" }),
        };
        assert!(command_runnable_unattended(&write));
        let dispatch = ToolCall {
            id: "c1".to_string(),
            name: "agent".to_string(),
            arguments: json!({ "role": "implementer", "task": "x" }),
        };
        assert!(command_runnable_unattended(&dispatch));
    }

    /// On Unix the gate is a no-op: `execve` resolves any command word, or fails
    /// the same way a shell would, so nothing is withheld from Parse authority.
    #[cfg(unix)]
    #[test]
    fn unix_runs_every_command_as_argv() {
        assert!(command_runnable_unattended(&shell("npm test")));
        assert!(command_runnable_unattended(&shell(
            "mkdir foo && touch foo/x"
        )));
    }

    /// On Windows a program word with no real `.exe`/`.com` cannot run as argv,
    /// so the gate withholds Parse authority and the command keeps prompting
    /// (running as text through `cmd /C`) instead of failing to spawn forever —
    /// the `26cbaf1` regression. `cmd.exe` is a real image and still passes.
    #[cfg(windows)]
    #[test]
    fn windows_withholds_argv_from_unresolvable_programs() {
        assert!(!command_runnable_unattended(&shell(
            "__no_such_program_xyz__ --flag"
        )));
        assert!(command_runnable_unattended(&shell("cmd /c echo hi")));
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

    /// A sub-agent dispatch prompt authorizes what its arguments say (a
    /// writing role, `network: true`), so "a" must not turn `agent` into a
    /// standing consent: one remembered dispatch would cover the other kind
    /// and skip the Auto egress floor, which standing consent sits above.
    #[test]
    fn subagent_dispatch_is_never_session_allowable() {
        assert!(!session_allowable("agent"));
    }

    /// The panel-side predicate agrees with the recording path: it is true
    /// exactly when "a" records something — a by-name consent, or a shell
    /// identity — and false wherever "a" would silently become a one-time
    /// approve (job cancel, a compound command, a dispatch, a root grant, an
    /// irreversible command).
    #[test]
    fn session_consent_recordable_mirrors_the_recording_rules() {
        assert!(session_consent_recordable(
            "write_file",
            &json!({ "path": "x", "content": "y" }),
            None
        ));
        assert!(session_consent_recordable(
            "shell",
            &json!({ "command": "cargo test --all" }),
            None
        ));
        assert!(session_consent_recordable(
            "job",
            &json!({ "action": "start", "command": "cargo test" }),
            None
        ));
        // A declared networked command still records its identity: egress is a
        // separate grant, remembered with its flag.
        assert!(session_consent_recordable(
            "shell",
            &json!({ "command": "git push origin main", "network": true }),
            None
        ));
        for (tool, arguments) in [
            ("shell", json!({ "command": "cargo test && rm -rf /" })),
            ("job", json!({ "action": "cancel", "job_id": "job_1" })),
            (
                "shell",
                json!({ "command": "python3", "stdin": "print(1)" }),
            ),
            (
                "job",
                json!({ "action": "start", "command": "python3", "interactive": true }),
            ),
            (
                "job",
                json!({ "action": "input", "job_id": "job_1", "stdin": "print(1)" }),
            ),
            ("job", json!({ "action": "close_stdin", "job_id": "job_1" })),
            ("agent", json!({ "role": "implementer", "task": "land it" })),
            (
                "agent",
                json!({ "role": "explore", "network": true, "task": "x" }),
            ),
            ("request_write_root", json!({ "path": "/tmp/x" })),
        ] {
            assert!(
                !session_consent_recordable(tool, &arguments, None),
                "{tool} {arguments} must not offer a session consent"
            );
        }
    }

    /// An irreversible command offers no session consent, and the reason is not
    /// theoretical: the gate refuses `HumanOnly` ABOVE session memory
    /// (`auto_approval_granted` returns before consulting it), so a recorded
    /// identity could never be read — "a" would print an option meaning "this
    /// once". The identity a shell consent records also drops flags, so a
    /// remembered force-push would have surfaced as plain `git push`.
    ///
    /// Pinned on the predicate the panel prints its options from AND on the
    /// decision the recording path makes: "the panel offered y/n" and "nothing
    /// was recorded" have to stay the same answer.
    #[tokio::test]
    async fn an_irreversible_command_records_no_session_consent() {
        const RULE: Option<crate::execution_policy::ApprovalAuthority> =
            Some(crate::execution_policy::ApprovalAuthority::HumanOnly);
        assert!(!session_consent_recordable(
            "shell",
            &json!({ "command": "git push --force origin main" }),
            RULE
        ));
        // The same command without the floor IS recordable, so the assertion
        // above is about the rule id rather than the command's shape.
        assert!(session_consent_recordable(
            "shell",
            &json!({ "command": "git push origin main" }),
            None
        ));

        // …and the recording path refuses even when a caller sends
        // `ApprovedForSession` for a floor call (a programmatic consumer can).
        let runtime = AgentRuntime::new(
            crate::echo_client::EchoClient::new(crate::i18n::SharedLang::default()),
            crate::tool::ToolRegistry::new(),
        );
        runtime.begin_turn("approve a shell call").await.unwrap();
        let pending = PendingToolBatch {
            approval_id: crate::ApprovalId::new(),
            current: crate::tool::ToolCall::new(
                "c1",
                "shell",
                json!({ "command": "git push --force origin main" }),
            ),
            remaining: std::collections::VecDeque::new(),
            turn_id: runtime.live_turn_id().await.unwrap(),
            cancel: runtime.state.lock().await.cancel.clone(),
            root_grant_target: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        runtime
            .handle_approval(pending, ApprovalDecision::ApprovedForSession, None, &tx)
            .await;
        let state = runtime.state.lock().await;
        assert!(
            state.session_trusted_shell_prefixes.is_empty(),
            "a floor call must record nothing: {:?}",
            state.session_trusted_shell_prefixes
        );
    }

    /// A session consent keyed on the tool name would approve all URLs or queries.
    #[test]
    fn network_native_tools_do_not_record_session_consent() {
        for (tool, arguments) in [
            ("fetch_url", json!({ "url": "https://docs.rs/serde" })),
            ("web_search", json!({ "query": "serde derive" })),
        ] {
            assert!(
                !session_allowable(tool),
                "{tool} must not be session-allowable by name"
            );
            assert!(
                !session_consent_recordable(tool, &arguments, None),
                "{tool} must not offer a session consent"
            );
        }
        assert!(is_network_tool("fetch_url"));
        assert!(is_network_tool("web_search"));
        // An ordinary tool still records one, so this did not disable the
        // feature wholesale.
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
}
