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
    /// installing dependencies). Routes the dispatch through user approval; an
    /// approved networked child gets the web tools (fetch_url, web_search) and
    /// its allow-listed sandboxed commands run with egress. Children without
    /// this grant have no network at all.
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
        let child_tools = child_tool_registry(
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
            child_config.model = model.to_string();
        }
        let runtime = AgentRuntime::with_system_prompt_shared(
            std::sync::Arc::clone(&self.services.client),
            child_tools,
            child_system_prompt(role, network),
            child_config,
            true,
        );

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
            run_subagent(runtime, DEFAULT_MAX_STEPS, role, progress).await
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
                        Err(_) => Err((0, reason)),
                    },
                    Err(_) => Err((0, reason)),
                }
            }
        };

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
            Err((_, message)) if message == "cancelled" => {
                let _ = manager.mark_cancelled(&agent_id);
                Ok(ToolOutput::soft_error("sub-agent cancelled"))
            }
            Err((steps, message)) => {
                let _ = manager.finalize_failure(&agent_id, message.clone(), steps);
                Ok(ToolOutput::soft_error(format!(
                    "sub-agent failed: {message}"
                )))
            }
        }
    }
}

/// A finished child run: `Ok(report, steps)` or `Err(steps, message)`.
type ChildOutcome = Result<(String, u32), (u32, String)>;

/// Flatten `catch_unwind`'s join result: a panic in the child run becomes a
/// failure with no steps recorded.
fn unwrap_panic(joined: Result<ChildOutcome, Box<dyn std::any::Any + Send>>) -> ChildOutcome {
    joined.unwrap_or_else(|_| Err((0, "sub-agent panicked".to_string())))
}
