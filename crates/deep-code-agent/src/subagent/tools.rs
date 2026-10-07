//! The model-facing `agent` tool: run a child agent to completion.
//!
//! One call is one child lifecycle — the tool blocks until the child finishes
//! and returns its structured report as the tool result. Parallelism comes
//! from issuing several `agent` calls in a single assistant turn, not from
//! detached sessions; there is nothing for the model to poll or clean up.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::runtime::AgentRuntime;
use crate::session_store::now_ms;
use crate::subagent::manager::new_agent_id;
use crate::subagent::registry::{SubAgentServices, child_system_prompt, child_tool_registry};
use crate::subagent::roles::SubAgentRole;
use crate::subagent::runner::run_subagent;
use crate::subagent::types::{DEFAULT_MAX_STEPS, SubAgentError, SubAgentRecord, SubAgentStatus};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolUpdate};
use crate::workspace_policy::invalid;

const AGENT_TOOL: &str = "agent";

/// Wall-clock ceiling for one child run. The step budget bounds work, not
/// time — without this, one child stuck on a slow model call would hang the
/// parent turn indefinitely.
const AGENT_WALL_CLOCK_TIMEOUT: Duration = Duration::from_secs(600);

/// After cancelling a child, how long to wait for it to unwind through its
/// own cancel check before abandoning the await.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

fn tool_error(error: SubAgentError) -> ToolError {
    ToolError::exec_failed(AGENT_TOOL, error.to_string())
}

pub struct AgentTool {
    services: std::sync::Arc<SubAgentServices>,
}

impl AgentTool {
    pub fn new(services: std::sync::Arc<SubAgentServices>) -> Self {
        Self { services }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentParams {
    /// Self-contained task brief for the child: the goal, relevant file or
    /// directory hints, and what the final report must answer. The child
    /// starts with a fresh context and sees nothing else.
    task: String,
    /// Capability profile: general | explore | plan | review | implementer |
    /// verifier (common aliases like explorer/reviewer/tester are accepted).
    /// Read-only roles cannot write files and run on the fast model tier.
    /// Defaults to general.
    role: Option<String>,
    /// Set true when the child task needs network access (fetching docs/URLs,
    /// installing dependencies, `git push`). Routes the dispatch through user
    /// approval; an approved networked child gets the web tools (fetch_url,
    /// web_search) and its allow-listed sandboxed commands run with egress.
    /// Children without this grant have no network at all. Note that the child
    /// also inherits this session's permission mode, so under yolo a child's
    /// gated calls are auto-approved exactly as the parent's are.
    network: Option<bool>,
    /// Optional display name (shown by /agents).
    name: Option<String>,
}

#[async_trait]
impl Tool for AgentTool {
    type Params = AgentParams;

    fn name(&self) -> &str {
        AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Run a focused child agent to completion and return its report. Blocks until the child \
         finishes; issue several agent calls in one turn to run children in parallel. Use for \
         investigations or delegated changes whose conclusion is much smaller than the work — \
         the child burns its own context, the parent only receives the report. Children have no \
         network unless dispatched with network=true (goes through user approval): a granted \
         child gets fetch_url/web_search and its allow-listed commands run with egress."
    }

    async fn run(&self, params: AgentParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let task = params.task.trim().to_string();
        if task.is_empty() {
            return Err(invalid(AGENT_TOOL, "task must not be empty"));
        }
        let role =
            SubAgentRole::parse(params.role.as_deref().unwrap_or("general")).map_err(tool_error)?;
        // Reaching execution IS the network consent, exactly like
        // `role.allows_writes()` for writes: a `network: true` dispatch only
        // gets here through its approval gate (or a mode/config the user chose
        // that waves it through — yolo, `[sandbox] network = "always"`, a
        // standing auto_allow). Under `network = "never"` the policy denies
        // the dispatch before this point.
        let network = params.network.unwrap_or(false);

        // Writing children are serialized to one. The manager's cap counts
        // children, which is the wrong unit: readers on one workspace are a
        // throughput choice, writers on one workspace are a corruption class.
        // Refused as a soft error rather than a hard one — nothing is wrong with
        // the call, it is just early, and the parent can retry after the running
        // writer finishes.
        //
        // The check-then-insert below is not itself atomic, and it does not need
        // to be, for a reason that lives in the policy engine: dispatching a
        // writing child always resolves to `NeedsApproval`, so it is never
        // `is_parallel_safe` and two of them cannot be in flight in one batch.
        // That is a real dependency on an invariant stated elsewhere — if a
        // future change let a writing dispatch be `Allow`, two writers in one
        // batch would both pass this check before either inserted.
        if role.allows_writes() {
            let running_writers = {
                let manager = self
                    .services
                    .manager
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                manager.running_writers()
            };
            if running_writers > 0 {
                return Ok(ToolOutput::soft_error(
                    "a writing sub-agent is already running, and writing children are \
                     serialized so two of them cannot fight over one workspace file, one \
                     package cache or one git index. Re-dispatch this task once it finishes \
                     (its record in /agents shows when)."
                        .to_string(),
                ));
            }
        }

        let agent_id = new_agent_id();
        let name = params
            .name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| agent_id.clone());

        // The child runtime renders its own strings from the child config's
        // language; the web tools follow the same source so a granted child's
        // fetch errors read in the same language as the rest of its session.
        let child_ui_lang = crate::i18n::SharedLang::new(crate::i18n::Lang::from_env(
            &self.services.agent_config.language,
        ));
        let (child_tools, child_jobs) = child_tool_registry(
            &self.services.boundary,
            role,
            self.services.exec_policy.clone(),
            network,
            &child_ui_lang,
        );
        // Reconnaissance roles run pinned to the flash tier (a fixed model id
        // bypasses per-turn auto-routing in the child); other roles inherit
        // the parent's configured model. See `SubAgentRole::model_override`.
        let mut child_config = self.services.agent_config.clone();
        if let Some(model) = role.model_override() {
            // Only substitute the built-in flash id when the parent is on a model
            // this build actually knows (a catalog id, or the `auto` sentinel).
            // On a custom `base_url` with a passthrough model id, `deepseek-flash`
            // does not exist upstream and every recon child would 404 — there,
            // inherit the parent's model instead. Mirrors `classifier_model_for`.
            let configured = child_config.model.trim();
            let known = configured.is_empty()
                || configured.eq_ignore_ascii_case(crate::model_registry::AUTO_MODEL)
                || crate::model_registry::ModelRegistry::default()
                    .info_for(configured)
                    .is_some();
            if known {
                child_config.model = model.to_string();
            }
        }
        let runtime = AgentRuntime::with_system_prompt_shared(
            std::sync::Arc::clone(&self.services.client),
            child_tools,
            child_system_prompt(role, network),
            child_config,
            true,
        )
        // The child shares the parent's mode handle (see
        // `SubAgentServices::permission_mode`). This is the one line that makes
        // "yolo" mean the same thing inside a dispatch as outside it.
        .with_permission_mode(self.services.permission_mode.clone());

        {
            let mut manager = self
                .services
                .manager
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manager
                .insert(SubAgentRecord {
                    agent_id: agent_id.clone(),
                    name: name.clone(),
                    role: role.as_str().to_string(),
                    status: SubAgentStatus::Running,
                    assignment: task.clone(),
                    result: None,
                    structured: None,
                    error: None,
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    steps_taken: 0,
                })
                .map_err(tool_error)?;
        }

        // A clone drives cancellation: cancelling `runtime` itself is
        // impossible once it is moved into the run future, so `cancel_handle`
        // (the same runtime, shared state) is what the timeout/cancel arms
        // signal through. `shutdown` fires when the whole session tears down.
        let cancel_handle = runtime.clone();
        let shutdown = self.services.parent_cancel.clone();
        // Forward the child's per-tool-call progress lines into the parent's
        // ToolCallProgress stream, so a long child run shows live activity in
        // the UI instead of a frozen `agent` cell.
        let progress = {
            let cx = cx.clone();
            move |text: String| {
                cx.update(ToolUpdate {
                    text,
                    details: None,
                });
            }
        };
        let run = std::panic::AssertUnwindSafe(async move {
            runtime.begin_turn(task).await;
            run_subagent(runtime, DEFAULT_MAX_STEPS, role, network, progress).await
        })
        .catch_unwind();
        tokio::pin!(run);

        let outcome: ChildOutcome = tokio::select! {
            joined = &mut run => unwrap_panic(joined),
            reason = async {
                tokio::select! {
                    () = tokio::time::sleep(AGENT_WALL_CLOCK_TIMEOUT) => format!(
                        "wall-clock timeout after {}s",
                        AGENT_WALL_CLOCK_TIMEOUT.as_secs()
                    ),
                    () = cx.cancel_token().cancelled() => "cancelled".to_string(),
                    () = shutdown.cancelled() => "cancelled".to_string(),
                }
            } => {
                // Stop the child's OWN turn loop, not just our wait: cancel_turn
                // cancels the child runtime's state token, which run_loop
                // observes and finalizes — so the detached loop stops streaming
                // and writing instead of running on as an orphan.
                cancel_handle.cancel_turn().await;
                // Honor a child that genuinely finished inside the grace window
                // (raced the deadline and won); otherwise the cancel/timeout
                // reason governs.
                match tokio::time::timeout(CANCEL_GRACE, &mut run).await {
                    Ok(joined) => match unwrap_panic(joined) {
                        Ok(success) => Ok(success),
                        Err(_) => Err((0, reason, None)),
                    },
                    Err(_) => Err((0, reason, None)),
                }
            }
        };

        // Kill any background jobs the child started. They outlive the child's
        // turn (the JobStore is not turn-scoped), so `cancel_turn` above does not
        // reach them — and nothing else holds this store, so without an explicit
        // shutdown its process groups only ever got `kill_on_drop`'s direct-child
        // SIGKILL, orphaning grandchildren (a child's `job start "cargo test"`
        // leaving test-binary subprocesses behind). Mirrors the main agent's
        // `RuntimeHandle::shutdown`.
        child_jobs.shutdown();

        // Fold the child's own request spend into the parent session totals: it
        // ran on the same API key, but its telemetry never reaches the parent
        // turn. Cache counters ride along so the session hit-rate/savings keep
        // covering every request billed to it. `cancel_handle` shares the
        // (now-finished) child's state.
        cx.report_spend(cancel_handle.session_spend().await);

        // Recover a poisoned lock rather than stranding this record as a zombie
        // Running entry: a prior panic under the lock must not block finalize.
        let mut manager = self
            .services
            .manager
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match outcome {
            Ok((text, steps)) => {
                let record = manager
                    .finalize_success(&agent_id, text.clone(), steps)
                    .map_err(tool_error)?;
                let mut output = ToolOutput::text(text);
                output.details = Some(json!({
                    "agent_id": record.agent_id,
                    "name": record.name,
                    "role": record.role,
                    "status": record.status.as_str(),
                    "steps": record.steps_taken,
                    "structured": record.structured,
                }));
                Ok(output)
            }
            Err((_, message, _)) if message == "cancelled" => {
                let _ = manager.mark_cancelled(&agent_id);
                Ok(ToolOutput::soft_error("sub-agent cancelled"))
            }
            Err((steps, message, partial)) => {
                let _ = manager.finalize_failure(&agent_id, message.clone(), steps);
                Ok(ToolOutput::soft_error(format!(
                    "sub-agent failed: {message}{}",
                    partial_report_suffix(partial.as_deref())
                )))
            }
        }
    }
}

/// Append whatever the child had already worked out, so an interrupted run is
/// something to continue from rather than something to redo.
///
/// The cap used to end a child with a one-line error and nothing else: the
/// parent's only sane response was re-dispatching the same task, which walked
/// the same steps again. The child's own last message is cheap to keep, and it
/// is labelled unverified because it never reached the `SUMMARY` contract.
fn partial_report_suffix(partial: Option<&str>) -> String {
    match partial {
        Some(text) => format!(
            "\n\nPartial report from the child (it stopped before it could report; this is its \
             last message, unverified, not a substitute for the output contract):\n{text}"
        ),
        None => String::new(),
    }
}

/// A finished child run: `Ok(report, steps)` or `Err(steps, message, partial)`.
type ChildOutcome = Result<(String, u32), (u32, String, Option<String>)>;

/// Flatten `catch_unwind`'s join result: a panic in the child run becomes a
/// failure with no steps recorded.
fn unwrap_panic(joined: Result<ChildOutcome, Box<dyn std::any::Any + Send>>) -> ChildOutcome {
    joined.unwrap_or_else(|_| Err((0, "sub-agent panicked".to_string(), None)))
}

#[cfg(test)]
mod tests {
    use super::partial_report_suffix;

    /// An interrupted child has to hand something back. A one-line failure is
    /// what made the parent's only sane move a full re-dispatch of a task that
    /// had already been half done.
    #[test]
    fn a_failure_carries_the_childs_partial_report() {
        let suffix = partial_report_suffix(Some("the parser lives at src/x.rs:40"));
        assert!(suffix.contains("src/x.rs:40"));
        assert!(
            suffix.contains("unverified"),
            "the label must not oversell an unreviewed message: {suffix}"
        );
    }

    /// "Nothing to hand over" must not read as an empty report.
    #[test]
    fn no_partial_report_adds_nothing() {
        assert!(partial_report_suffix(None).is_empty());
    }
}
