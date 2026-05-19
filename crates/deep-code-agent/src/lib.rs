//! deep-code agent core library.

mod client;
mod config;
mod error;
mod event;
mod message;
mod model;
mod runtime;
mod session;
mod tool;
mod workspace_tools;

pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use config::{AgentConfig, DEFAULT_DEEPSEEK_BASE_URL, DEFAULT_DEEPSEEK_MODEL};
pub use error::{AgentError, AgentResult};
pub use event::{AgentEvent, chunk_to_events};
pub use message::{Message, Role};
pub use model::{
    ChatChoice, ChatRequest, ChatTool, ChatToolFunction, ChoiceDelta, FunctionCallDelta,
    StreamChunk, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload, Usage,
};
pub use runtime::{AgentRuntime, AgentRuntimeHandle, RuntimeEvent, RuntimeEventReceiver};
pub use session::Session;
pub use tool::{
    ApprovalDecision, ApprovalRequest, MockEchoTool, Tool, ToolCall, ToolCallAccumulator,
    ToolError, ToolRegistry, ToolResult, ToolResultStatus, ToolRunOutcome, ToolSpec,
};
pub use workspace_tools::{WorkspaceTools, workspace_tool_registry};
