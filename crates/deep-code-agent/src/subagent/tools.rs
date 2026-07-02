use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::LlmClient;
use crate::runtime::AgentRuntime;
use crate::subagent::manager::{SubAgentManager, new_agent_id, now_ms};
use crate::subagent::registry::{SubAgentServices, child_system_prompt, child_tool_registry};
use crate::subagent::roles::SubAgentRole;
use crate::subagent::runner::run_subagent;
use crate::subagent::types::{
    DEFAULT_EVAL_TIMEOUT_MS, MAX_SYNC_EVAL_WAIT_MS, SUBAGENT_STATE_SCHEMA_VERSION, SubAgentError,
    SubAgentRecord, SubAgentStatus,
};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput};
use crate::workspace_policy::invalid;

const OPEN_TOOL: &str = "agent_open";
const EVAL_TOOL: &str = "agent_eval";
const CLOSE_TOOL: &str = "agent_close";

fn tool_error(error: SubAgentError) -> ToolError {
    ToolError::ExecutionFailed {
        name: "subagent".to_string(),
        message: error.to_string(),
    }
}

pub struct AgentOpenTool<C: LlmClient + Clone + 'static> {
    services: std::sync::Arc<SubAgentServices<C>>,
}

impl<C: LlmClient + Clone + 'static> AgentOpenTool<C> {
    pub fn new(services: std::sync::Arc<SubAgentServices<C>>) -> Self {
        Self { services }
    }
}

/// Alias-tolerant params: the model may send `message`/`objective` for
/// `prompt`, `agent_type`/`role` for `type`, `session_name` for `name`.
/// Everything is Option so the alias resolution stays in `run`; the wire
/// schema (with `required: ["prompt"]`) is the hand-written override below.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentOpenParams {
    name: Option<String>,
    session_name: Option<String>,
    prompt: Option<String>,
    message: Option<String>,
    objective: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    agent_type: Option<String>,
    role: Option<String>,
    fork_context: Option<bool>,
}

#[async_trait]
impl<C: LlmClient + Clone + 'static> Tool for AgentOpenTool<C> {
    type Params = AgentOpenParams;

    fn name(&self) -> &str {
        OPEN_TOOL
    }

    fn description(&self) -> &str {
        "Open a named child sub-agent session for focused background work. Returns agent_id, status, and transcript_handle metadata. Use agent_eval to wait/fetch and agent_close to cancel."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Stable session name"},
                "session_name": {"type": "string", "description": "Alias for name"},
                "prompt": {"type": "string", "description": "Initial task for the child"},
                "message": {"type": "string", "description": "Alias for prompt"},
                "objective": {"type": "string", "description": "Alias for prompt"},
                "type": {"type": "string", "description": "Role: general, explore, plan, review, implementer, verifier"},
                "agent_type": {"type": "string", "description": "Alias for type"},
                "role": {"type": "string", "description": "Alias for type"},
                "fork_context": {"type": "boolean", "description": "Reserved; fresh context is used in v1"}
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    async fn run(&self, params: AgentOpenParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let prompt = params
            .prompt
            .or(params.message)
            .or(params.objective)
            .ok_or_else(|| invalid(OPEN_TOOL, "missing task prompt"))?;
        let role_raw = params
            .type_
            .or(params.agent_type)
            .or(params.role)
            .unwrap_or_else(|| "general".to_string());
        let role = SubAgentRole::parse(&role_raw).map_err(tool_error)?;
        let fork_context = params.fork_context.unwrap_or(false);
        let name = params
            .name
            .or(params.session_name)
            .unwrap_or_default();

        let agent_id = new_agent_id();
        let session_name = if name.is_empty() {
            agent_id.clone()
        } else {
            name
        };

        let child_tools = child_tool_registry(
            &self.services.workspace,
            role,
            self.services.exec_policy.clone(),
        )
        .map_err(|error| ToolError::ExecutionFailed {
            name: OPEN_TOOL.to_string(),
            message: error.to_string(),
        })?;
        let system_prompt = child_system_prompt(role);
        let client = std::sync::Arc::clone(&self.services.client);
        let runtime = AgentRuntime::with_system_prompt(
            (*client).clone(),
            child_tools,
            system_prompt,
            self.services.agent_config.clone(),
            true,
        );
        let cancel = self.services.parent_cancel.child_token();
        {
            let mut cancels = self.services.agent_cancels.write().map_err(|error| {
                ToolError::ExecutionFailed {
                    name: OPEN_TOOL.to_string(),
                    message: error.to_string(),
                }
            })?;
            cancels.insert(agent_id.clone(), cancel.clone());
        }

        let boot_id = {
            let manager =
                self.services
                    .manager
                    .read()
                    .map_err(|error| ToolError::ExecutionFailed {
                        name: OPEN_TOOL.to_string(),
                        message: error.to_string(),
                    })?;
            manager.session_boot_id.clone()
        };

        let record = SubAgentRecord {
            schema_version: SUBAGENT_STATE_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            name: session_name.clone(),
            role: role.as_str().to_string(),
            status: SubAgentStatus::Running,
            assignment: prompt.clone(),
            result: None,
            structured: None,
            transcript_handle: None,
            error: None,
            fork_context,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            steps_taken: 0,
            session_boot_id: Some(boot_id),
        };

        {
            let mut manager =
                self.services
                    .manager
                    .write()
                    .map_err(|error| ToolError::ExecutionFailed {
                        name: OPEN_TOOL.to_string(),
                        message: error.to_string(),
                    })?;
            manager.insert(record.clone()).map_err(tool_error)?;
            if let Some(stored) = manager.get(&agent_id).cloned()
                && let Ok(handle) = manager.store_transcript(&stored)
            {
                let _ = manager.update(&agent_id, |record| {
                    record.transcript_handle = Some(handle.id);
                });
            }
        }

        let services = std::sync::Arc::clone(&self.services);
        let assignment = prompt.clone();
        let spawned_id = agent_id.clone();
        tokio::spawn(async move {
            runtime.begin_turn(assignment).await;
            let outcome = run_subagent(
                client,
                runtime,
                cancel.clone(),
                super::runner::default_max_steps(),
            )
            .await;
            if let Ok(mut map) = services.agent_cancels.write() {
                map.remove(&spawned_id);
            }
            let mut manager = match services.manager.write() {
                Ok(manager) => manager,
                Err(_) => return,
            };
            match outcome {
                Ok((text, steps)) => {
                    let _ = manager.finalize_success(&spawned_id, text, steps);
                }
                Err(message) if message == "cancelled" => {
                    let _ = manager.update(&spawned_id, |record| {
                        record.status = SubAgentStatus::Cancelled;
                        record.error = Some(message);
                        record.finished_at_ms = Some(now_ms());
                    });
                    if let Some(record) = manager.get(&spawned_id).cloned() {
                        let _ = manager.store_transcript(&record).map(|handle| {
                            let _ = manager.update(&spawned_id, |record| {
                                record.transcript_handle = Some(handle.id);
                            });
                        });
                    }
                }
                Err(message) => {
                    let _ = manager.finalize_failure(&spawned_id, message, 0);
                }
            }
        });

        let manager = self
            .services
            .manager
            .read()
            .map_err(|error| ToolError::ExecutionFailed {
                name: OPEN_TOOL.to_string(),
                message: error.to_string(),
            })?;
        let record = manager.get(&agent_id).expect("inserted");
        let projection = manager.project(record, false).map_err(tool_error)?;
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&projection).unwrap_or_default(),
        ))
    }
}

pub struct AgentEvalTool<C: LlmClient + Clone + 'static> {
    services: std::sync::Arc<SubAgentServices<C>>,
}

impl<C: LlmClient + Clone + 'static> AgentEvalTool<C> {
    pub fn new(services: std::sync::Arc<SubAgentServices<C>>) -> Self {
        Self { services }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentEvalParams {
    agent_id: Option<String>,
    /// Session name from agent_open
    name: Option<String>,
    /// Alias for name
    session_name: Option<String>,
    /// Wait until terminal (default false; blocking wait capped in sync tool path)
    wait: Option<bool>,
    /// Wait timeout in milliseconds when wait=true
    timeout_ms: Option<u64>,
}

#[async_trait]
impl<C: LlmClient + Clone + 'static> Tool for AgentEvalTool<C> {
    type Params = AgentEvalParams;

    fn name(&self) -> &str {
        EVAL_TOOL
    }

    fn description(&self) -> &str {
        "Fetch or wait for a child sub-agent session by agent_id or name. Returns the session projection with transcript_handle metadata. Prefer wait=false and poll; blocking wait is capped to avoid stalling the parent loop."
    }

    async fn run(&self, params: AgentEvalParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let agent_id = params.agent_id;
        let name = params.name.or(params.session_name);
        if agent_id.is_none() && name.is_none() {
            return Err(invalid(EVAL_TOOL, "agent_id or name is required"));
        }
        let wait = params.wait.unwrap_or(false);
        let timeout_ms = params
            .timeout_ms
            .unwrap_or(DEFAULT_EVAL_TIMEOUT_MS)
            .min(if wait {
                MAX_SYNC_EVAL_WAIT_MS
            } else {
                DEFAULT_EVAL_TIMEOUT_MS
            });

        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        loop {
            {
                let manager =
                    self.services
                        .manager
                        .read()
                        .map_err(|error| ToolError::ExecutionFailed {
                            name: EVAL_TOOL.to_string(),
                            message: error.to_string(),
                        })?;
                let record = find_record(&manager, agent_id.as_deref(), name.as_deref())
                    .ok_or_else(|| ToolError::ExecutionFailed {
                        name: EVAL_TOOL.to_string(),
                        message: SubAgentError::NotFound {
                            id: agent_id.clone().or(name.clone()).unwrap_or_default(),
                        }
                        .to_string(),
                    })?
                    .clone();
                if record.status.is_terminal() || !wait {
                    let projection = manager.project(&record, timed_out).map_err(tool_error)?;
                    return Ok(ToolOutput::text(
                        serde_json::to_string_pretty(&projection).unwrap_or_default(),
                    ));
                }
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let record = find_record(&manager, agent_id.as_deref(), name.as_deref())
                        .expect("record")
                        .clone();
                    let projection = manager.project(&record, timed_out).map_err(tool_error)?;
                    return Ok(ToolOutput::text(
                        serde_json::to_string_pretty(&projection).unwrap_or_default(),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub struct AgentCloseTool<C: LlmClient + Clone + 'static> {
    services: std::sync::Arc<SubAgentServices<C>>,
}

impl<C: LlmClient + Clone + 'static> AgentCloseTool<C> {
    pub fn new(services: std::sync::Arc<SubAgentServices<C>>) -> Self {
        Self { services }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentCloseParams {
    agent_id: Option<String>,
    name: Option<String>,
    session_name: Option<String>,
}

#[async_trait]
impl<C: LlmClient + Clone + 'static> Tool for AgentCloseTool<C> {
    type Params = AgentCloseParams;

    fn name(&self) -> &str {
        CLOSE_TOOL
    }

    fn description(&self) -> &str {
        "Close a child sub-agent session, cancelling it when still running."
    }

    async fn run(&self, params: AgentCloseParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let agent_id = params.agent_id;
        let name = params.name.or(params.session_name);
        let resolved_id = {
            let manager =
                self.services
                    .manager
                    .read()
                    .map_err(|error| ToolError::ExecutionFailed {
                        name: CLOSE_TOOL.to_string(),
                        message: error.to_string(),
                    })?;
            find_record(&manager, agent_id.as_deref(), name.as_deref())
                .ok_or_else(|| ToolError::ExecutionFailed {
                    name: CLOSE_TOOL.to_string(),
                    message: "sub-agent not found".to_string(),
                })?
                .agent_id
                .clone()
        };
        if let Ok(cancels) = self.services.agent_cancels.read()
            && let Some(token) = cancels.get(&resolved_id)
        {
            token.cancel();
        }
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: CLOSE_TOOL.to_string(),
                    message: error.to_string(),
                })?;
        let record = manager.mark_cancelled(&resolved_id).map_err(tool_error)?;
        let projection = manager.project(&record, false).map_err(tool_error)?;
        manager
            .release_transcript_handles(&record)
            .map_err(tool_error)?;
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&projection).unwrap_or_default(),
        ))
    }
}

fn find_record<'a>(
    manager: &'a SubAgentManager,
    agent_id: Option<&str>,
    name: Option<&str>,
) -> Option<&'a SubAgentRecord> {
    if let Some(id) = agent_id {
        return manager.get(id);
    }
    let name = name?;
    manager
        .list_current_session()
        .iter()
        .find(|record| record.name == name)
        .and_then(|record| manager.get(&record.agent_id))
}
