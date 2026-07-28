mod accumulator;
mod mock;
mod registry;
mod schema;

pub use accumulator::ToolCallAccumulator;
pub use mock::MockEchoTool;
pub use registry::ToolRegistry;

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::execution_policy::{
    ExecPolicy, RiskLevel, SafetyNote, ToolExecutionPlan, ToolKind, safety_notes,
};
use crate::model::{ChatTool, ChatToolFunction};
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
    /// Sink for token cost a tool incurs out-of-band — spend the parent turn's
    /// telemetry never sees, e.g. a sub-agent's own requests on the shared key.
    /// The runtime folds it into the session total after the tool returns.
    cost_sink: Option<Arc<std::sync::Mutex<crate::pricing::CostEstimate>>>,
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
    pub fn with_cost_sink(mut self, sink: Arc<std::sync::Mutex<crate::pricing::CostEstimate>>) -> Self {
        self.cost_sink = Some(sink);
        self
    }

    /// Report token cost this tool incurred that the parent turn's telemetry
    /// won't otherwise capture (e.g. a sub-agent's own request spend). Summed
    /// into the sink; the runtime folds it into the session total afterward.
    pub fn report_cost(&self, cost: crate::pricing::CostEstimate) {
        if let Some(sink) = &self.cost_sink {
            let mut total = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            total.usd += cost.usd;
            total.cny += cost.cny;
        }
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

impl ToolError {
    /// Build an [`ExecutionFailed`](ToolError::ExecutionFailed) without spelling
    /// out the struct fields at every call site.
    pub(crate) fn exec_failed(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ExecutionFailed {
            name: tool.into(),
            message: message.into(),
        }
    }
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
        Err(join_error) => Err(ToolError::exec_failed(
            tool_name,
            format!("tool execution task failed: {join_error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::PolicyVerdict;
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
    fn tool_cx_cost_sink_accumulates_reported_cost() {
        use crate::pricing::CostEstimate;
        // No sink → report_cost is a silent no-op (most tools never report).
        ToolCx::new().report_cost(CostEstimate { usd: 1.0, cny: 7.0 });

        let sink = Arc::new(std::sync::Mutex::new(CostEstimate::default()));
        let cx = ToolCx::new().with_cost_sink(Arc::clone(&sink));
        cx.report_cost(CostEstimate { usd: 0.5, cny: 3.5 });
        cx.report_cost(CostEstimate { usd: 0.25, cny: 1.75 });
        let total = *sink.lock().unwrap();
        assert!((total.usd - 0.75).abs() < 1e-9 && (total.cny - 5.25).abs() < 1e-9);
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
}
