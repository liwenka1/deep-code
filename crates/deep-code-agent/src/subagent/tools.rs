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

use crate::client::LlmClient;
use crate::runtime::AgentRuntime;
use crate::subagent::manager::{new_agent_id, now_ms};
use crate::subagent::registry::{SubAgentServices, child_system_prompt, child_tool_registry};
use crate::subagent::roles::SubAgentRole;
use crate::subagent::runner::run_subagent;
use crate::subagent::types::{
    DEFAULT_MAX_STEPS, SUBAGENT_STATE_SCHEMA_VERSION, SubAgentError, SubAgentRecord, SubAgentStatus,
};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput};
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
    ToolError::ExecutionFailed {
        name: AGENT_TOOL.to_string(),
        message: error.to_string(),
    }
}

pub struct AgentTool<C: LlmClient + Clone + 'static> {
    services: std::sync::Arc<SubAgentServices<C>>,
}

impl<C: LlmClient + Clone + 'static> AgentTool<C> {
    pub fn new(services: std::sync::Arc<SubAgentServices<C>>) -> Self {
        Self { services }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentParams {
    /// Self-contained task brief for the child: the goal, relevant file or
    /// directory hints, and what the final report must answer. The child
    /// starts with a fresh context and sees nothing else.
    task: String,
    /// Capability profile: general | explore | plan | review | implementer |
    /// verifier. Read-only roles cannot write files. Defaults to general.
    role: Option<String>,
    /// Optional display name (shown by /agents).
    name: Option<String>,
}

#[async_trait]
impl<C: LlmClient + Clone + 'static> Tool for AgentTool<C> {
    type Params = AgentParams;

    fn name(&self) -> &str {
        AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Run a focused child agent to completion and return its report. Blocks until the child \
         finishes; issue several agent calls in one turn to run children in parallel. Use for \
         investigations or delegated changes whose conclusion is much smaller than the work — \
         the child burns its own context, the parent only receives the report."
    }

    async fn run(&self, params: AgentParams, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let task = params.task.trim().to_string();
        if task.is_empty() {
            return Err(invalid(AGENT_TOOL, "task must not be empty"));
        }
        let role =
            SubAgentRole::parse(params.role.as_deref().unwrap_or("general")).map_err(tool_error)?;

        let agent_id = new_agent_id();
        let name = params
            .name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| agent_id.clone());

        let child_tools = child_tool_registry(
            &self.services.workspace,
            role,
            self.services.exec_policy.clone(),
        )
        .map_err(|error| ToolError::ExecutionFailed {
            name: AGENT_TOOL.to_string(),
            message: error.to_string(),
        })?;
        let runtime = AgentRuntime::with_system_prompt(
            (*self.services.client).clone(),
            child_tools,
            child_system_prompt(role),
            self.services.agent_config.clone(),
            true,
        );

        let boot_id = {
            let manager = self.services.manager.read().map_err(poisoned)?;
            manager.session_boot_id.clone()
        };
        {
            let mut manager = self.services.manager.write().map_err(poisoned)?;
            manager
                .insert(SubAgentRecord {
                    schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
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
                    session_boot_id: Some(boot_id),
                })
                .map_err(tool_error)?;
        }

        // Two cancellation parents: the runtime-wide token (session shutdown)
        // via child_token, and the turn token (user pressed cancel) via the
        // select below.
        let child_cancel = self.services.parent_cancel.child_token();
        let run = std::panic::AssertUnwindSafe(async {
            runtime.begin_turn(task).await;
            run_subagent(runtime, child_cancel.clone(), DEFAULT_MAX_STEPS, role).await
        })
        .catch_unwind();
        tokio::pin!(run);

        let unwrap_panic = |joined: Result<Result<(String, u32), String>, _>| match joined {
            Ok(inner) => inner,
            Err(_) => Err("sub-agent panicked".to_string()),
        };
        let outcome = tokio::select! {
            joined = &mut run => unwrap_panic(joined),
            () = tokio::time::sleep(AGENT_WALL_CLOCK_TIMEOUT) => {
                child_cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut run).await;
                Err(format!(
                    "wall-clock timeout after {}s",
                    AGENT_WALL_CLOCK_TIMEOUT.as_secs()
                ))
            }
            () = cx.cancel_token().cancelled() => {
                child_cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut run).await;
                Err("cancelled".to_string())
            }
        };

        let mut manager = self.services.manager.write().map_err(poisoned)?;
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
            Err(message) if message == "cancelled" => {
                let _ = manager.mark_cancelled(&agent_id);
                Ok(ToolOutput::soft_error("sub-agent cancelled"))
            }
            Err(message) => {
                let _ = manager.finalize_failure(&agent_id, message.clone(), 0);
                Ok(ToolOutput::soft_error(format!(
                    "sub-agent failed: {message}"
                )))
            }
        }
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> ToolError {
    ToolError::ExecutionFailed {
        name: AGENT_TOOL.to_string(),
        message: "sub-agent manager lock poisoned".to_string(),
    }
}
