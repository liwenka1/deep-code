//! deep-code agent core library.

mod client;
mod config;
mod error;
mod event;
mod checkpoint;
mod execution_policy;
mod git_tools;
mod sandbox;
mod workspace_policy;
mod message;
mod model;
mod runtime;
mod session;
mod shell_tools;
mod tool;
mod tool_execution;
mod workspace_tools;

pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use config::{AgentConfig, DEFAULT_DEEPSEEK_BASE_URL, DEFAULT_DEEPSEEK_MODEL};
pub use error::{AgentError, AgentResult};
pub use checkpoint::{CheckpointId, CheckpointStore};
pub use event::{AgentEvent, chunk_to_events};
pub use execution_policy::{
    ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind, evaluate_shell_command,
};
pub use git_tools::{GitTools, git_tool_registry};
pub use sandbox::{
    SandboxBackend, SandboxCapabilities, SandboxManager, SandboxPolicy, capabilities,
    detect_capabilities,
};
pub use message::{Message, Role};
pub use model::{
    ChatChoice, ChatRequest, ChatTool, ChatToolFunction, ChoiceDelta, FunctionCallDelta,
    StreamChunk, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload, Usage,
};
pub use runtime::{AgentRuntime, AgentRuntimeHandle, RuntimeEvent, RuntimeEventReceiver};
pub use session::Session;
pub use shell_tools::{ShellTools, shell_tool_registry};
pub use tool::{
    ApprovalDecision, ApprovalRequest, MockEchoTool, Tool, ToolCall, ToolCallAccumulator,
    ToolError, ToolRegistry, ToolResult, ToolResultStatus, ToolRunOutcome, ToolSpec,
};
pub use workspace_tools::{WorkspaceTools, workspace_tool_registry};
