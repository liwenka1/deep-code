mod schema;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{
    ExecPolicy, PolicyVerdict, RiskLevel, SafetyNote, ToolExecutionPlan, ToolKind, safety_notes,
};
use crate::message::Message;
use crate::model::{ChatTool, ChatToolFunction, FunctionCallDelta, ToolCallDelta};
use crate::sandbox::SandboxPolicy;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Structured data for UI rendering; never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
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
            details: None,
        }
    }

    #[must_use]
    pub fn denied(call: &ToolCall) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolResultStatus::Denied,
            content: "Tool call denied by user.".to_string(),
            details: None,
        }
    }

    #[must_use]
    pub fn error(call: &ToolCall, message: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolResultStatus::Error,
            content: message.into(),
            details: None,
        }
    }

    #[must_use]
    pub fn to_message(&self) -> Message {
        Message::tool(self.call_id.clone(), self.content.clone())
    }
}

/// Incremental progress payload a tool can emit while running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUpdate {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

pub type ToolUpdateFn = Arc<dyn Fn(ToolUpdate) + Send + Sync>;

/// Execution context for one tool invocation.
///
/// Replaces the old `tool_execution` thread-local: the sandbox plan travels
/// explicitly with the invocation, so it survives `.await` points and can be
/// cloned into `spawn_blocking` closures.
#[derive(Clone, Default)]
pub struct ToolCx {
    cancel: CancellationToken,
    plan: Option<ToolExecutionPlan>,
    on_update: Option<ToolUpdateFn>,
}

impl ToolCx {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    #[must_use]
    pub fn with_update_fn(mut self, on_update: ToolUpdateFn) -> Self {
        self.on_update = Some(on_update);
        self
    }

    #[must_use]
    pub(crate) fn with_plan(mut self, plan: ToolExecutionPlan) -> Self {
        self.plan = Some(plan);
        self
    }

    #[must_use]
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    #[must_use]
    pub fn plan(&self) -> Option<&ToolExecutionPlan> {
        self.plan.as_ref()
    }

    /// Sandbox policy for this invocation (Unsandboxed when no plan is set),
    /// mirroring the old `tool_execution::current_sandbox_policy()` semantics.
    #[must_use]
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        SandboxPolicy::from_execution_plan(self.plan.as_ref())
    }

    pub fn update(&self, update: ToolUpdate) {
        if let Some(on_update) = &self.on_update {
            on_update(update);
        }
    }

    pub fn update_text(&self, text: impl Into<String>) {
        self.update(ToolUpdate {
            text: text.into(),
            details: None,
        });
    }
}

/// What a tool produces: model-facing `content`, UI-facing `details`.
///
/// `status` exists because some tools report soft failures as a normal result
/// (status=Error) rather than a `ToolError` — e.g. web tools returning an
/// unreachable-URL message the model should read and react to.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub status: ToolResultStatus,
    pub content: String,
    pub details: Option<Value>,
}

impl ToolOutput {
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Success,
            content: content.into(),
            details: None,
        }
    }

    /// A failure the model should read and recover from, recorded as a
    /// status=Error result without aborting the tool pipeline.
    #[must_use]
    pub fn soft_error(content: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Error,
            content: content.into(),
            details: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn into_result(self, call_id: &str, tool_name: &str) -> ToolResult {
        ToolResult {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            status: self.status,
            content: self.content,
            details: self.details,
        }
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
    /// Human-reviewable change preview (unified diff for file mutations),
    /// filled by the runtime before the request is surfaced. Raw arguments
    /// alone are not enough to approve a large rewrite safely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Static advisory notes for shell commands as language-neutral keys (why
    /// this warrants review + a paired suggestion); the UI renders them in the
    /// user's language. Not a dry-run — see [`crate::execution_policy::safety_notes`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safety_notes: Vec<SafetyNote>,
}

/// Static safety notes for a shell-bearing tool call (the `shell` tool, or
/// `job` with `action=start`). Empty for every other tool. Advisory only —
/// surfaced at the approval prompt, never a gate.
fn shell_safety_notes(call: &ToolCall) -> Vec<SafetyNote> {
    let command = match ExecPolicy::classify_tool(&call.name) {
        ToolKind::Shell => call.arguments.get("command").and_then(Value::as_str),
        ToolKind::Job if call.arguments.get("action").and_then(Value::as_str) == Some("start") => {
            call.arguments.get("command").and_then(Value::as_str)
        }
        _ => None,
    };
    command.map(safety_notes).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    /// Approve now and remember the tool for the rest of the session
    /// (recorded by the runtime; the registry treats it as a plain approve).
    /// Shell-class tools are downgraded to a one-time approve.
    ApprovedForSession,
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

/// A tool with typed, schema-derived parameters.
///
/// `Params` is the single source of truth: schemars derives the wire schema,
/// serde parses and validates the arguments before [`Tool::run`] is invoked.
///
/// `run` executes on the async runtime — wrap blocking work (fs, subprocess
/// wait, blocking HTTP) in [`run_blocking`] so it lands on the blocking pool.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    type Params: DeserializeOwned + JsonSchema + Send + 'static;

    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn requires_approval(&self) -> bool {
        false
    }

    /// Wire schema for the params. Override only when the generated schema
    /// must diverge from the derive (hand-tuned `oneOf`, alias-aware
    /// `required` lists); parsing still goes through `Params`.
    fn parameters(&self) -> Value {
        schema::parameters_schema::<Self::Params>()
    }

    async fn run(&self, params: Self::Params, cx: &ToolCx) -> Result<ToolOutput, ToolError>;
}

/// Object-safe tool interface the registry stores.
///
/// Every [`Tool`] gets this via the blanket impl. Implement it directly only
/// for tools whose schema is not known at compile time.
#[async_trait]
pub trait ErasedTool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn execute(&self, call: &ToolCall, cx: &ToolCx) -> Result<ToolResult, ToolError>;
}

#[async_trait]
impl<T: Tool> ErasedTool for T {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
            requires_approval: self.requires_approval(),
        }
    }

    async fn execute(&self, call: &ToolCall, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let params: T::Params =
            serde_json::from_value(call.arguments.clone()).map_err(|error| {
                ToolError::InvalidArguments {
                    name: call.name.clone(),
                    message: error.to_string(),
                }
            })?;
        let output = self.run(params, cx).await?;
        Ok(output.into_result(&call.id, &call.name))
    }
}

/// Run a blocking tool body on the blocking pool.
///
/// Cancellation note: the closure runs to completion even if the caller is
/// dropped — check `ToolCx::is_cancelled` inside long loops where feasible.
pub async fn run_blocking<T>(
    tool_name: &str,
    body: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(body).await {
        Ok(result) => result,
        Err(join_error) => Err(ToolError::ExecutionFailed {
            name: tool_name.to_string(),
            message: format!("tool execution task failed: {join_error}"),
        }),
    }
}

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

#[derive(Debug, Clone, Copy)]
pub struct MockEchoTool;

impl MockEchoTool {
    pub const NAME: &'static str = "mock_echo";
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MockEchoParams {
    /// Message to echo back.
    message: String,
}

#[async_trait]
impl Tool for MockEchoTool {
    type Params = MockEchoParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Safely echoes a message to validate the tool loop."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: MockEchoParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("mock_echo: {}", params.message)))
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
    use serde_json::json;

    #[tokio::test]
    async fn policy_denies_dangerous_shell_before_execution() {
        let workspace = tempfile::tempdir().unwrap();
        let (registry, _) = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
        let call = ToolCall::new("call_deny", "shell", json!({"command": "rm -rf /"}));
        let plan = registry.evaluate_tool(&call);
        let outcome = registry
            .run_tool_call_with_plan(&call, None, plan, ToolCx::new())
            .await
            .unwrap();
        let ToolRunOutcome::Result { result } = outcome else {
            panic!("expected denied result");
        };
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(result.content.contains("execution policy denied"));
    }

    #[tokio::test]
    async fn mock_tool_requires_approval_then_executes_after_approval() {
        let registry = ToolRegistry::with_mock_tools();
        let call = ToolCall::new(
            "call_1",
            MockEchoTool::NAME,
            json!({"message": "hello tools"}),
        );

        let pending = registry.run_tool_call(call.clone(), None).await.unwrap();
        let ToolRunOutcome::ApprovalRequired { request } = pending else {
            panic!("expected approval request");
        };
        assert_eq!(request.call_id, "call_1");
        assert_eq!(request.tool_name, MockEchoTool::NAME);
        assert!(request.description.contains("approval"));

        let executed = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .await
            .unwrap();
        assert_eq!(
            executed,
            ToolRunOutcome::Result {
                result: ToolResult::success("call_1", MockEchoTool::NAME, "mock_echo: hello tools")
            }
        );
    }

    #[tokio::test]
    async fn denied_tool_call_becomes_tool_result_message() {
        let registry = ToolRegistry::with_mock_tools();
        let call = ToolCall::new("call_2", MockEchoTool::NAME, json!({"message": "nope"}));

        let ToolRunOutcome::Result { result } = registry
            .run_tool_call(call, Some(ApprovalDecision::Denied))
            .await
            .unwrap()
        else {
            panic!("expected denied result");
        };

        assert_eq!(result.status, ToolResultStatus::Denied);
        assert_eq!(result.to_message(), Message::tool("call_2", result.content));
    }

    #[tokio::test]
    async fn blanket_impl_rejects_invalid_arguments_before_run() {
        let registry = ToolRegistry::with_mock_tools();
        // message must be a string; 42 fails serde validation in the blanket
        // impl before MockEchoTool::run is ever invoked.
        let call = ToolCall::new("call_bad", MockEchoTool::NAME, json!({"message": 42}));
        let error = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn tool_cx_sandbox_policy_mirrors_execution_plan() {
        // No plan → Unsandboxed (old thread-local default).
        assert_eq!(ToolCx::new().sandbox_policy(), SandboxPolicy::Unsandboxed);

        let plan = |requires_sandbox: bool, read_only: bool| ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox,
            read_only,
            risk_level: RiskLevel::Low,
            matched_rule: None,
        };

        assert_eq!(
            ToolCx::new().with_plan(plan(true, true)).sandbox_policy(),
            SandboxPolicy::ReadOnly
        );
        assert_eq!(
            ToolCx::new().with_plan(plan(true, false)).sandbox_policy(),
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
        );
        assert_eq!(
            ToolCx::new().with_plan(plan(false, false)).sandbox_policy(),
            SandboxPolicy::Unsandboxed
        );
    }

    #[test]
    fn generated_schemas_keep_function_calling_invariants() {
        let workspace = tempfile::tempdir().unwrap();
        let mut registry =
            crate::workspace_tools::workspace_tool_registry(workspace.path()).unwrap();
        let (shell, _) = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
        registry.extend(shell);

        for spec in registry.specs() {
            let schema = &spec.parameters;
            assert_eq!(schema["type"], "object", "{}: type", spec.name);
            assert_eq!(
                schema["additionalProperties"],
                Value::Bool(false),
                "{}: additionalProperties",
                spec.name
            );
            let text = schema.to_string();
            assert!(!text.contains("$schema"), "{}: $schema leaked", spec.name);
            assert!(!text.contains("$ref"), "{}: $ref leaked", spec.name);
            assert!(
                schema["properties"].is_object(),
                "{}: properties missing",
                spec.name
            );
        }
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
