use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::execution_policy::{ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan};
use crate::message::Message;
use crate::model::{ChatTool, ChatToolFunction, FunctionCallDelta, ToolCallDelta};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub requires_approval: bool,
}

impl ToolSpec {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        requires_approval: bool,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            requires_approval,
        }
    }

    #[must_use]
    pub fn to_chat_tool(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: ChatToolFunction {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Denied,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub content: String,
}

impl ToolResult {
    #[must_use]
    pub fn success(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            status: ToolResultStatus::Success,
            content: content.into(),
        }
    }

    #[must_use]
    pub fn denied(call: &ToolCall) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolResultStatus::Denied,
            content: "Tool call denied by user.".to_string(),
        }
    }

    #[must_use]
    pub fn error(call: &ToolCall, message: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolResultStatus::Error,
            content: message.into(),
        }
    }

    #[must_use]
    pub fn to_message(&self) -> Message {
        Message::tool(self.call_id.clone(), self.content.clone())
    }
}

fn default_risk_level() -> RiskLevel {
    RiskLevel::Low
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool_name: String,
    pub description: String,
    pub arguments: Value,
    #[serde(default = "default_risk_level")]
    pub risk_level: RiskLevel,
    #[serde(default)]
    pub requires_sandbox: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRunOutcome {
    ApprovalRequired { request: ApprovalRequest },
    Result { result: ToolResult },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ToolError {
    #[error("unknown tool: {name}")]
    UnknownTool { name: String },

    #[error("invalid tool arguments for {name}: {message}")]
    InvalidArguments { name: String, message: String },

    #[error("tool execution failed for {name}: {message}")]
    ExecutionFailed { name: String, message: String },
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    policy: ExecPolicy,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_policy(policy: ExecPolicy) -> Self {
        Self {
            tools: HashMap::new(),
            policy,
        }
    }

    #[must_use]
    pub fn policy(&self) -> &ExecPolicy {
        &self.policy
    }

    pub fn set_policy(&mut self, policy: ExecPolicy) {
        self.policy = policy;
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.spec().name, Arc::new(tool));
    }

    #[must_use]
    pub fn with_mock_tools() -> Self {
        let mut registry = Self::new();
        registry.register(MockEchoTool);
        registry
    }

    pub fn extend(&mut self, other: ToolRegistry) {
        self.tools.extend(other.tools);
    }

    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self
            .tools
            .values()
            .map(|tool| tool.spec())
            .collect::<Vec<_>>();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    #[must_use]
    pub fn chat_tools(&self) -> Vec<ChatTool> {
        self.specs()
            .into_iter()
            .map(|spec| spec.to_chat_tool())
            .collect()
    }

    pub fn run_tool_call(
        &self,
        call: ToolCall,
        decision: Option<ApprovalDecision>,
    ) -> Result<ToolRunOutcome, ToolError> {
        self.run_tool_call_with_plan(
            &call,
            decision,
            self.policy.evaluate_tool(&call.name, &call.arguments),
        )
    }

    pub fn evaluate_tool(&self, call: &ToolCall) -> ToolExecutionPlan {
        self.policy.evaluate_tool(&call.name, &call.arguments)
    }

    pub fn run_tool_call_with_plan(
        &self,
        call: &ToolCall,
        decision: Option<ApprovalDecision>,
        plan: ToolExecutionPlan,
    ) -> Result<ToolRunOutcome, ToolError> {
        let tool = self
            .tools
            .get(&call.name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool {
                name: call.name.clone(),
            })?;
        let spec = tool.spec();

        if let Some(reason) = plan.denied_reason() {
            return Ok(ToolRunOutcome::Result {
                result: ToolResult::error(call, format!("execution policy denied: {reason}")),
            });
        }

        let needs_approval = plan.requires_approval || spec.requires_approval;
        if needs_approval {
            let description = match &plan.verdict {
                PolicyVerdict::NeedsApproval { reason } => reason.clone(),
                _ => spec.description.clone(),
            };
            match decision {
                None => {
                    return Ok(ToolRunOutcome::ApprovalRequired {
                        request: ApprovalRequest {
                            call_id: call.id.clone(),
                            tool_name: spec.name,
                            description,
                            arguments: call.arguments.clone(),
                            risk_level: plan.risk_level,
                            requires_sandbox: plan.requires_sandbox,
                            read_only: plan.read_only,
                            matched_rule: plan.matched_rule.clone(),
                        },
                    });
                }
                Some(ApprovalDecision::Denied) => {
                    return Ok(ToolRunOutcome::Result {
                        result: ToolResult::denied(call),
                    });
                }
                Some(ApprovalDecision::Approved) => {}
            }
        }

        Ok(ToolRunOutcome::Result {
            result: crate::tool_execution::with_plan(plan, || tool.execute(call))?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MockEchoTool;

impl MockEchoTool {
    pub const NAME: &'static str = "mock_echo";
}

impl Tool for MockEchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Self::NAME,
            "Safely echoes a message to validate the tool loop.",
            json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Message to echo back."
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let message = call
            .arguments
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments {
                name: call.name.clone(),
                message: "missing string field 'message'".to_string(),
            })?;

        Ok(ToolResult::success(
            call.id.clone(),
            call.name.clone(),
            format!("mock_echo: {message}"),
        ))
    }
}

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: HashMap<u32, PartialToolCall>,
}

impl ToolCallAccumulator {
    pub fn push_delta(&mut self, delta: ToolCallDelta) {
        let index = delta.index.unwrap_or(0);
        let call = self.calls.entry(index).or_default();

        if let Some(id) = delta.id {
            call.id = Some(id);
        }

        if let Some(FunctionCallDelta { name, arguments }) = delta.function {
            if let Some(name) = name {
                call.name = Some(name);
            }

            if let Some(arguments) = arguments {
                call.arguments.push_str(&arguments);
            }
        }
    }

    pub fn finish(self) -> Result<Vec<ToolCall>, ToolError> {
        let mut calls = self.calls.into_iter().collect::<Vec<_>>();
        calls.sort_by_key(|(index, _)| *index);

        calls
            .into_iter()
            .map(|(index, call)| {
                let id = call.id.unwrap_or_else(|| format!("call_{index}"));
                let name = call.name.ok_or_else(|| ToolError::InvalidArguments {
                    name: id.clone(),
                    message: "missing function name".to_string(),
                })?;
                let arguments = if call.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&call.arguments).map_err(|error| {
                        ToolError::InvalidArguments {
                            name: name.clone(),
                            message: error.to_string(),
                        }
                    })?
                };

                Ok(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_denies_dangerous_shell_before_execution() {
        let workspace = tempfile::tempdir().unwrap();
        let registry = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
        let call = ToolCall::new("call_deny", "shell_run", json!({"command": "rm -rf /"}));
        let plan = registry.evaluate_tool(&call);
        let outcome = registry.run_tool_call_with_plan(&call, None, plan).unwrap();
        let ToolRunOutcome::Result { result } = outcome else {
            panic!("expected denied result");
        };
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("execution policy denied"));
    }

    #[test]
    fn mock_tool_requires_approval_then_executes_after_approval() {
        let registry = ToolRegistry::with_mock_tools();
        let call = ToolCall::new(
            "call_1",
            MockEchoTool::NAME,
            json!({"message": "hello tools"}),
        );

        let pending = registry.run_tool_call(call.clone(), None).unwrap();
        let ToolRunOutcome::ApprovalRequired { request } = pending else {
            panic!("expected approval request");
        };
        assert_eq!(request.call_id, "call_1");
        assert_eq!(request.tool_name, MockEchoTool::NAME);
        assert!(request.description.contains("approval"));

        let executed = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap();
        assert_eq!(
            executed,
            ToolRunOutcome::Result {
                result: ToolResult::success("call_1", MockEchoTool::NAME, "mock_echo: hello tools")
            }
        );
    }

    #[test]
    fn denied_tool_call_becomes_tool_result_message() {
        let registry = ToolRegistry::with_mock_tools();
        let call = ToolCall::new("call_2", MockEchoTool::NAME, json!({"message": "nope"}));

        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Denied))
            .unwrap()
        else {
            panic!("expected denied result");
        };

        assert_eq!(result.status, ToolResultStatus::Denied);
        assert_eq!(result.to_message(), Message::tool("call_2", result.content));
    }

    #[test]
    fn accumulator_builds_tool_call_from_streaming_deltas() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push_delta(ToolCallDelta {
            index: Some(0),
            id: Some("call_3".to_string()),
            call_type: Some("function".to_string()),
            function: Some(FunctionCallDelta {
                name: Some(MockEchoTool::NAME.to_string()),
                arguments: Some(r#"{"message":"hel"#.to_string()),
            }),
        });
        accumulator.push_delta(ToolCallDelta {
            index: Some(0),
            id: None,
            call_type: None,
            function: Some(FunctionCallDelta {
                name: None,
                arguments: Some(r#"lo"}"#.to_string()),
            }),
        });

        assert_eq!(
            accumulator.finish().unwrap(),
            vec![ToolCall::new(
                "call_3",
                MockEchoTool::NAME,
                json!({"message": "hello"})
            )]
        );
    }
}
