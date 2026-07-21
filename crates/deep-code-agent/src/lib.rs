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
mod lsp;
mod message;
mod model;
mod model_registry;
mod paths;
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

// Public surface is intentionally narrow: only what the sibling crates (TUI,
// runtime API, eval) actually consume, plus types reachable through those
// items' signatures. Keeping it tight lets rustc's dead_code lint see the
// rest of the crate. Re-add an export the day a consumer needs it.
pub use checkpoint::{CheckpointId, CheckpointStore};
pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use config::{
    AgentConfig, ConfigLayer, ConfigLoadReport, ConfigSources, GlobalConfigUpdate,
    LoadedAgentConfig, validate_api_key, write_global_config_update,
};
pub use doctor::{ConfigLayersDoctorReport, DoctorReport, default_config_path};
pub use error::AgentResult;
pub use event::AgentEvent;
pub use execution_policy::{ExecPolicy, PolicyVerdict, RiskLevel, evaluate_shell_command};
pub use lsp::{
    Diagnostic, DiagnosticRange, Language, LspConfig, LspManager, LspTransport, Severity,
    render_blocks,
};
pub use message::Message;
pub use model::{
    ChatRequest, FunctionCallDelta, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload, Usage,
};
pub use model_registry::{DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelRegistry};
pub use pricing::{CostCurrency, CostEstimate, PrefixStatus, TurnTelemetry};
pub use runtime::{
    AgentRuntime, AgentRuntimeHandle, RuntimeEvent, RuntimeEventReceiver, ToolCallId, TurnId,
};
pub use runtime_launch::{LaunchedRuntime, launch_runtime, web_enabled};
pub use sandbox::detect_capabilities;
pub use session::Session;
pub use session_entry::{EntryKind, ExchangeResult, SessionEntry, ToolExchange};
pub use session_store::{
    CheckpointRecord, ConfigSnapshot, JsonSessionStore, SessionId, SessionRecord, SessionStore,
    SessionStoreError, TurnRecord, format_sessions_storage_note,
};
pub use shell_tools::JobStore;
pub use subagent::{SharedSubAgentManager, SubAgentManager, is_subagent_tool};
pub use tool::{
    ApprovalDecision, ApprovalRequest, MockEchoTool, ToolError, ToolRegistry, ToolResult,
    ToolResultStatus,
};
pub use workspace_summary::list_workspace_files;
