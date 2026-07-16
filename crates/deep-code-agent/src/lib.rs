//! deep-code agent core library.

mod approval_preview;
mod auto_mode;
mod checkpoint;
mod client;
mod compaction;
mod config;
mod doctor;
mod echo_client;
mod error;
mod event;
mod execution_policy;
mod extensions;
mod hooks;
mod lsp;
mod mcp;
mod message;
mod model;
mod model_registry;
mod paths;
mod plan_mode;
mod pricing;
mod reasoning;
mod runtime;
mod runtime_launch;
mod sandbox;
mod session;
mod session_entry;
mod session_store;
mod shell_tools;
mod skills;
mod subagent;
mod task_class;
mod tool;
mod web_tools;
mod workspace_policy;
mod workspace_summary;
mod workspace_tools;

pub use auto_mode::{
    TurnRoute, api_fallback_model, resolve_turn_route, select_auto_model,
    select_auto_model_with_reason,
};
pub use checkpoint::{CheckpointId, CheckpointStore};
pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use compaction::{
    CompactionResult, compact_entries, context_usage_percent, effective_compaction_threshold,
    estimate_token_count, near_compaction_threshold, should_compact, stable_prefix_fingerprint,
};
pub use config::{
    AUTO_COST_SAVING_ENV, AgentConfig, COMPACTION_THRESHOLD_ENV, COST_CURRENCY_ENV, ConfigLayer,
    ConfigLayerStatus, ConfigLoadReport, ConfigSources, DEEPSEEK_API_KEY_ENV,
    DEFAULT_DEEPSEEK_BASE_URL, DEFAULT_DEEPSEEK_MODEL, GlobalConfigUpdate, LoadedAgentConfig,
    MODEL_ENV, REASONING_EFFORT_ENV, validate_api_key, write_global_config_update,
};
pub use doctor::{ConfigLayersDoctorReport, DoctorReport, default_config_path};
pub use echo_client::EchoClient;
pub use error::{AgentError, AgentResult, api_key_setup_hint};
pub use event::{AgentEvent, chunk_to_events};
pub use execution_policy::{
    ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind, evaluate_shell_command,
};
pub use extensions::{
    AgentExtensions, RuntimeBootstrap, attach_agent_extensions, attach_runtime_tools,
    build_runtime_system_prompt,
};
pub use hooks::{
    HookDispatcher, HookError, HookEvent, HookSink, HooksConfig, JsonlHookSink, StdoutHookSink,
    ToolGate, ToolInterceptor, default_hooks_config_path, load_hooks_config,
};
pub use lsp::{
    Diagnostic, DiagnosticBlock, DiagnosticRange, Language, LspConfig, LspManager, LspTransport,
    Severity, StdioLspTransport, detect_language, is_edit_tool, normalize_path, paths_equal,
    render_blocks, summarize_blocks,
};
pub use mcp::{
    InMemoryMcpClient, McpConfigFile, McpError, McpManager, McpServerConfig, McpServerEntry,
    McpServerStatus, McpServerSummary, McpTransport, McpValidationReport, default_mcp_config_path,
    is_mcp_tool_name, load_mcp_config, qualify_tool_name, register_mcp_tools, set_server_enabled,
    workspace_mcp_config_path,
};
pub use message::{Message, Role};
pub use model::{
    ChatChoice, ChatRequest, ChatTool, ChatToolFunction, ChoiceDelta, FunctionCallDelta,
    StreamChunk, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload, Usage,
};
pub use model_registry::{
    AUTO_MODEL, DEEPSEEK_V4_CONTEXT_WINDOW, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelInfo,
    ModelRegistry, ModelResolution, compaction_threshold_for_model, context_window_for_model,
};
pub use plan_mode::{PlanMode, PlanModeInterceptor};
pub use pricing::{CostCurrency, CostEstimate, PrefixStatus, TurnTelemetry, calculate_turn_cost};
pub use reasoning::{ReasoningEffort, ReasoningEffortSetting, select_auto_effort};
pub use runtime::{
    AgentRuntime, AgentRuntimeHandle, RuntimeEvent, RuntimeEventReceiver, ToolCallId, TurnId,
};
pub use runtime_launch::{
    DEFAULT_SYSTEM_PROMPT, LaunchedRuntime, build_tool_registry, launch_runtime,
    runtime_system_prompt,
};
pub use sandbox::{
    SandboxBackend, SandboxCapabilities, SandboxGuard, SandboxManager, SandboxPolicy, capabilities,
    detect_capabilities,
};
pub use session::Session;
pub use session_entry::{EntryId, EntryKind, ExchangeResult, SessionEntry, ToolExchange};
pub use session_store::{
    CheckpointRecord, ConfigSnapshot, JsonSessionStore, SESSION_SCHEMA_VERSION, SessionId,
    SessionRecord, SessionStore, SessionStoreError, TurnRecord, format_sessions_storage_note,
    new_session_id, sessions_dir_for_workspace, validate_session_id,
};
pub use shell_tools::{BackgroundJobSummary, JobStore, ShellTools, shell_tool_registry};
pub use skills::{
    Skill, SkillRegistry, build_system_prompt, discover_in_workspace, global_skills_dir,
    render_skills_block, skills_directories, workspace_skills_dir,
};
pub use subagent::{
    AgentCloseTool, AgentEvalTool, AgentOpenTool, DEFAULT_MAX_CONCURRENT, HARD_MAX_CONCURRENT,
    SharedSubAgentManager, StructuredReport, SubAgentManager, SubAgentRecord, SubAgentRole,
    SubAgentServices, SubAgentSessionProjection, SubAgentStatus, attach_subagent_tools,
    is_subagent_tool, register_subagent_tools, subagent_tool_registry,
};
pub use tool::{
    ApprovalDecision, ApprovalRequest, ErasedTool, MockEchoTool, Tool, ToolCall,
    ToolCallAccumulator, ToolCx, ToolError, ToolOutput, ToolRegistry, ToolResult, ToolResultStatus,
    ToolRunOutcome, ToolSpec, ToolUpdate, ToolUpdateFn, run_blocking,
};
pub use web_tools::{FetchUrlTool, WebSearchTool, web_tool_registry};
pub use workspace_summary::{build_workspace_summary, list_workspace_files};
pub use workspace_tools::{WorkspaceTools, workspace_tool_registry};
