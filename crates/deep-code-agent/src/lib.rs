//! deep-code agent core library.

mod client;
mod config;
mod doctor;
mod echo_client;
mod error;
mod event;
mod checkpoint;
mod execution_policy;
mod extensions;
mod git_tools;
mod handle;
mod hooks;
mod lsp;
mod mcp;
mod rlm;
mod sandbox;
mod skills;
mod subagent;
mod workspace_policy;
mod message;
mod model;
mod runtime;
mod runtime_launch;
mod session;
mod session_store;
mod shell_tools;
mod tool;
mod tool_execution;
mod workspace_tools;

pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use echo_client::EchoClient;
pub use config::{AgentConfig, DEFAULT_DEEPSEEK_BASE_URL, DEFAULT_DEEPSEEK_MODEL};
pub use doctor::{DoctorReport, default_config_path};
pub use error::{AgentError, AgentResult};
pub use checkpoint::{CheckpointId, CheckpointStore};
pub use event::{AgentEvent, chunk_to_events};
pub use execution_policy::{
    ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind, evaluate_shell_command,
};
pub use git_tools::{GitTools, git_tool_registry};
pub use lsp::{
    Diagnostic, DiagnosticBlock, DiagnosticRange, Language, LspConfig, LspManager, LspTransport,
    Severity, StdioLspTransport, detect_language, is_edit_tool, normalize_path, paths_equal,
    render_blocks, summarize_blocks,
};
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
pub use runtime_launch::{
    LaunchedRuntime, build_tool_registry, launch_runtime, runtime_system_prompt,
    DEFAULT_SYSTEM_PROMPT,
};
pub use session::Session;
pub use session_store::{
    ConfigSnapshot, JsonSessionStore, SessionId, SessionRecord, SessionStore,
    SessionStoreError, TurnRecord, SESSION_SCHEMA_VERSION, new_session_id,
    sessions_dir_for_workspace, validate_session_id, format_sessions_storage_note,
};
pub use extensions::{
    AgentExtensions, RuntimeBootstrap, attach_agent_extensions, attach_runtime_tools,
    build_runtime_system_prompt,
};
pub use hooks::{
    HookDispatcher, HookError, HookEvent, HookSink, HooksConfig, JsonlHookSink, StdoutHookSink,
    default_hooks_config_path, load_hooks_config,
};
pub use mcp::{
    InMemoryMcpClient, McpConfigFile, McpError, McpManager, McpServerConfig, McpServerEntry,
    McpServerStatus, McpServerSummary, McpTransport, McpValidationReport, default_mcp_config_path,
    is_mcp_tool_name, load_mcp_config, qualify_tool_name, register_mcp_tools,
    set_server_enabled, workspace_mcp_config_path,
};
pub use skills::{
    Skill, SkillRegistry, build_system_prompt, discover_in_workspace, global_skills_dir,
    render_skills_block, skills_directories, workspace_skills_dir,
};
pub use handle::{
    HandleCount, HandleId, HandleKind, HandleReadOutput, HandleRecord, HandleStore, HandleSummary,
    VarHandle, HANDLE_READ_TOOL, HandleReadTool, register_handle_read,
};
pub use rlm::{
    RlmCloseTool, RlmConfigureTool, RlmEvalTool, RlmManager, RlmOpenTool, RlmConfig, RlmServices,
    RlmSessionInfo, RLM_TOOL_NAMES, is_rlm_tool, register_rlm_tools,
};
pub use subagent::{
    AgentCloseTool, AgentEvalTool, AgentOpenTool, DEFAULT_MAX_CONCURRENT, HARD_MAX_CONCURRENT,
    SharedSubAgentManager, StructuredReport, SubAgentManager, SubAgentRole, SubAgentServices,
    SubAgentSessionProjection, SubAgentStatus, attach_subagent_tools, is_subagent_tool,
    register_subagent_tools, subagent_tool_registry,
};
pub use shell_tools::{ShellTools, shell_tool_registry};
pub use tool::{
    ApprovalDecision, ApprovalRequest, MockEchoTool, Tool, ToolCall, ToolCallAccumulator,
    ToolError, ToolRegistry, ToolResult, ToolResultStatus, ToolRunOutcome, ToolSpec,
};
pub use workspace_tools::{WorkspaceTools, workspace_tool_registry};
