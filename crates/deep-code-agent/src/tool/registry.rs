//! The tool registry: stores erased tools, evaluates policy, and drives one
//! tool call (including the approval gate) to a [`ToolRunOutcome`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::execution_policy::{ExecPolicy, PolicyVerdict, ToolExecutionPlan};
use crate::model::ChatTool;

use super::{
    ApprovalDecision, ApprovalRequest, ErasedTool, MockEchoTool, Tool, ToolCall, ToolCx, ToolError,
    ToolResult, ToolRunOutcome, ToolSpec, shell_safety_notes,
};

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    tool: Arc<dyn ErasedTool>,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
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

    pub fn register<T: Tool>(&mut self, tool: T) {
        self.register_erased(Arc::new(tool));
    }

    /// Register a dynamically-shaped tool. The spec (including the schemars
    /// run for typed tools) is computed once here, not per request.
    pub fn register_erased(&mut self, tool: Arc<dyn ErasedTool>) {
        let spec = tool.spec();
        self.tools
            .insert(spec.name.clone(), RegisteredTool { spec, tool });
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

    /// Clone a subset of tools from another registry.
    #[must_use]
    pub fn filtered_from(source: &ToolRegistry, predicate: impl Fn(&str) -> bool) -> Self {
        let mut registry = Self::new();
        registry.policy = source.policy.clone();
        for (name, entry) in &source.tools {
            if predicate(name) {
                registry.tools.insert(name.clone(), entry.clone());
            }
        }
        registry
    }

    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self
            .tools
            .values()
            .map(|entry| entry.spec.clone())
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

    pub async fn run_tool_call(
        &self,
        call: ToolCall,
        decision: Option<ApprovalDecision>,
    ) -> Result<ToolRunOutcome, ToolError> {
        let plan = self.policy.evaluate_tool(&call.name, &call.arguments);
        self.run_tool_call_with_plan(&call, decision, plan, ToolCx::new())
            .await
    }

    pub fn evaluate_tool(&self, call: &ToolCall) -> ToolExecutionPlan {
        self.policy.evaluate_tool(&call.name, &call.arguments)
    }

    pub async fn run_tool_call_with_plan(
        &self,
        call: &ToolCall,
        decision: Option<ApprovalDecision>,
        plan: ToolExecutionPlan,
        cx: ToolCx,
    ) -> Result<ToolRunOutcome, ToolError> {
        let entry = self
            .tools
            .get(&call.name)
            .ok_or_else(|| ToolError::UnknownTool {
                name: call.name.clone(),
            })?;
        let spec = &entry.spec;

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
                    let notes = shell_safety_notes(call);
                    return Ok(ToolRunOutcome::ApprovalRequired {
                        request: ApprovalRequest {
                            call_id: call.id.clone(),
                            tool_name: spec.name.clone(),
                            description,
                            arguments: call.arguments.clone(),
                            risk_level: plan.risk_level,
                            requires_sandbox: plan.requires_sandbox,
                            read_only: plan.read_only,
                            matched_rule: plan.matched_rule.clone(),
                            // Filled by the runtime (needs workspace access).
                            preview: None,
                            safety_notes: notes,
                        },
                    });
                }
                Some(ApprovalDecision::Denied) => {
                    return Ok(ToolRunOutcome::Result {
                        result: ToolResult::denied(call),
                    });
                }
                Some(ApprovalDecision::Approved | ApprovalDecision::ApprovedForSession) => {}
            }
        }

        let cx = cx.with_plan(plan);
        entry
            .tool
            .execute(call, &cx)
            .await
            .map(|result| ToolRunOutcome::Result { result })
    }
}
