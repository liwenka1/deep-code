mod accumulator;
// NOTE: this is a test fixture, but it cannot be `cfg(test)`-gated: the
// `deep-code-runtime` tests are a separate crate and consume it, so gating would
// need a `test-fixtures` Cargo feature plus conditional `ToolKind::Mock` match
// arms. Judged not worth that complexity for one ~30-line tool; it is inert in
// production (never registered outside `with_mock_tools`).
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

use crate::execution_policy::{RiskLevel, SafetyNote, ToolExecutionPlan, safety_notes};
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

    /// The shell command this call would run, if it is command-bearing (the
    /// `shell` tool, or `job` with `action=start`); `None` otherwise. Delegates
    /// to [`crate::execution_policy::shell_command_of`] — the single home for
    /// that rule.
    #[must_use]
    pub fn shell_command(&self) -> Option<&str> {
        crate::execution_policy::shell_command_of(&self.name, &self.arguments)
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

/// Out-of-band spend one tool run reports back to the runtime — requests the
/// parent turn's telemetry never sees (e.g. a sub-agent's own turns on the
/// shared key). Cache traffic rides along with the cost so folding keeps the
/// session's hit-rate and savings covering every request billed to it, the
/// same accounting `record_classifier_cost` uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolSpend {
    pub cost: crate::pricing::CostEstimate,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_savings: crate::pricing::CostEstimate,
}

impl ToolSpend {
    /// Whether anything was reported at all (savings can only accompany hits,
    /// so the cache counters and cost cover every field).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.cost.usd == 0.0
            && self.cost.cny == 0.0
            && self.cache_hit_tokens == 0
            && self.cache_miss_tokens == 0
    }
}

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
    /// Sink for spend a tool incurs out-of-band — requests the parent turn's
    /// telemetry never sees, e.g. a sub-agent's own turns on the shared key.
    /// The runtime folds it into the session totals after the tool returns.
    spend_sink: Option<Arc<std::sync::Mutex<ToolSpend>>>,
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
    pub fn with_spend_sink(mut self, sink: Arc<std::sync::Mutex<ToolSpend>>) -> Self {
        self.spend_sink = Some(sink);
        self
    }

    /// Report spend this tool incurred that the parent turn's telemetry won't
    /// otherwise capture (e.g. a sub-agent's own requests), cache traffic
    /// included. Summed into the sink; the runtime folds it into the session
    /// totals afterward.
    pub fn report_spend(&self, spend: ToolSpend) {
        if let Some(sink) = &self.spend_sink {
            let mut total = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            total.cost.usd += spend.cost.usd;
            total.cost.cny += spend.cost.cny;
            total.cache_hit_tokens += spend.cache_hit_tokens;
            total.cache_miss_tokens += spend.cache_miss_tokens;
            total.cache_savings.usd += spend.cache_savings.usd;
            total.cache_savings.cny += spend.cache_savings.cny;
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
    /// The call declares it needs network access (`network: true`), so
    /// approving runs it with egress/listening enabled. Surfaced as a badge;
    /// AcceptEdits and the Auto judge never auto-approve a declaration.
    #[serde(default)]
    pub network: bool,
    /// The model's stated reason for the call (`justification` argument),
    /// verbatim. Shown at the prompt labelled as the model's claim — it is
    /// advisory for the human and never an input to any auto-approval path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// For `request_write_root`: the canonical directory the grant would
    /// actually widen to, resolved when this prompt was built. The panel
    /// must judge by THIS path — the raw `path` argument may be a symlink
    /// spelling of somewhere else — and the runtime refuses the grant unless
    /// the request still resolves to this exact value at approval time.
    /// Runtime-filled; `None` for every other tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_target: Option<String>,
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
    /// user's language. Not a dry-run — see `execution_policy::safety_notes`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safety_notes: Vec<SafetyNote>,
}

/// Static safety notes for a shell-bearing tool call (the `shell` tool, or
/// `job` with `action=start`). Empty for every other tool. Advisory only —
/// surfaced at the approval prompt, never a gate.
fn shell_safety_notes(call: &ToolCall) -> Vec<SafetyNote> {
    call.shell_command().map(safety_notes).unwrap_or_default()
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

impl ApprovalDecision {
    /// The localization key for this decision's label. On the enum, like
    /// `RiskLevel::text_id`, so every surface that names a decision — the y/a/n
    /// keypress and the runtime's `ApprovalResolved` event alike — renders the
    /// same localized word instead of a `format!("{:?}")` of the variant.
    #[must_use]
    pub fn text_id(self) -> crate::i18n::TextId {
        match self {
            Self::Approved => crate::i18n::TextId::DecisionApproved,
            Self::ApprovedForSession => crate::i18n::TextId::DecisionApprovedSession,
            Self::Denied => crate::i18n::TextId::DecisionDenied,
        }
    }
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

    /// The payload without the `tool execution failed for <name>: ` framing.
    ///
    /// For re-wrapping one `ToolError` inside another: `format!("{error}")`
    /// there would stutter the prefix, which is how `restore`'s failure advice
    /// came out reading `tool execution failed for checkpoint: tool execution
    /// failed for checkpoint: …`.
    pub(crate) fn message(&self) -> &str {
        match self {
            // NOT the name: `UnknownTool`'s Display is "unknown tool: {name}",
            // which carries no `tool execution failed for …` framing to strip.
            // Returning `name` handed the caller a bare tool name and deleted
            // the cause, so a re-wrap would have read "checkpoint; the
            // workspace is partially cleared…". Unreachable from today's two
            // call sites (both build `ExecutionFailed`), but this reads as a
            // general accessor and the next re-wrap would inherit it.
            Self::UnknownTool { .. } => "unknown tool",
            Self::InvalidArguments { message, .. } | Self::ExecutionFailed { message, .. } => {
                message
            }
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

    /// A `justification` argument reaches the approval request verbatim (any
    /// tool), and its absence stays `None` — the field is the model's claim
    /// for the human, extracted from the raw arguments.
    #[tokio::test]
    async fn approval_request_carries_the_models_justification() {
        let workspace = tempfile::tempdir().unwrap();
        let (registry, _) = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
        let with = ToolCall::new(
            "call_1",
            "shell",
            json!({
                "command": "cargo fetch",
                "network": true,
                "justification": "  needs crates.io to resolve deps  ",
            }),
        );
        let ToolRunOutcome::ApprovalRequired { request } =
            registry.run_tool_call(with, None).await.unwrap()
        else {
            panic!("network declaration must require approval");
        };
        assert!(request.network);
        assert_eq!(
            request.justification.as_deref(),
            Some("needs crates.io to resolve deps"),
            "trimmed claim reaches the prompt"
        );

        let without = ToolCall::new("call_2", "shell", json!({"command": "cargo fetch"}));
        let ToolRunOutcome::ApprovalRequired { request } =
            registry.run_tool_call(without, None).await.unwrap()
        else {
            panic!("untrusted shell prompts");
        };
        assert_eq!(request.justification, None);
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

        let plan = |requires_sandbox: bool, network: bool| ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: None,
            network,
        };

        assert_eq!(
            ToolCx::new().with_plan(plan(true, true)).sandbox_policy(),
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
        );
        assert_eq!(
            ToolCx::new().with_plan(plan(true, false)).sandbox_policy(),
            SandboxPolicy::WorkspaceWrite {
                network_access: false
            }
        );
        assert_eq!(
            ToolCx::new().with_plan(plan(false, false)).sandbox_policy(),
            SandboxPolicy::Unsandboxed
        );
    }

    #[test]
    fn tool_cx_spend_sink_accumulates_reported_spend() {
        use crate::pricing::CostEstimate;
        // No sink → report_spend is a silent no-op (most tools never report).
        ToolCx::new().report_spend(ToolSpend {
            cost: CostEstimate { usd: 1.0, cny: 7.0 },
            ..ToolSpend::default()
        });

        let sink = Arc::new(std::sync::Mutex::new(ToolSpend::default()));
        let cx = ToolCx::new().with_spend_sink(Arc::clone(&sink));
        cx.report_spend(ToolSpend {
            cost: CostEstimate { usd: 0.5, cny: 3.5 },
            cache_hit_tokens: 100,
            cache_miss_tokens: 40,
            cache_savings: CostEstimate { usd: 0.1, cny: 0.7 },
        });
        cx.report_spend(ToolSpend {
            cost: CostEstimate {
                usd: 0.25,
                cny: 1.75,
            },
            cache_hit_tokens: 20,
            cache_miss_tokens: 10,
            cache_savings: CostEstimate {
                usd: 0.05,
                cny: 0.35,
            },
        });
        let total = *sink.lock().unwrap();
        assert!((total.cost.usd - 0.75).abs() < 1e-9 && (total.cost.cny - 5.25).abs() < 1e-9);
        assert_eq!(total.cache_hit_tokens, 120);
        assert_eq!(total.cache_miss_tokens, 50);
        assert!((total.cache_savings.usd - 0.15).abs() < 1e-9);
        assert!(!total.is_zero());
        assert!(ToolSpend::default().is_zero());
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
