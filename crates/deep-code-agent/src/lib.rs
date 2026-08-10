//! deep-code agent core library.

mod approval_classifier;
mod approval_preview;
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
pub mod i18n;
mod lsp;
mod message;
mod model;
mod model_registry;
mod model_route;
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
mod text_util;
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
pub use client::{AgentEventStream, LlmClient};
pub use config::{
    AgentConfig, ConfigLoadReport, ConfigSources, GlobalConfigUpdate, LoadedAgentConfig,
    validate_api_key, write_global_config_update,
};
pub use doctor::{DoctorReport, default_config_path};
pub use error::AgentResult;
pub use event::AgentEvent;
pub use execution_policy::{
    NetworkMode, PermissionMode, RiskLevel, SafetyNote, SharedPermissionMode,
};
pub use i18n::{Lang, TextId, tr, tr_with};
// Already reachable through `AgentRuntime::session_messages`'s signature;
// exported so consumers (the headless driver) can actually name them.
pub use message::{Message, Role};
pub use model::{
    ChatRequest, FunctionCallDelta, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload, Usage,
};
pub use model_registry::{DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelRegistry};
pub use pricing::{CostCurrency, CostEstimate};
pub use runtime::{
    AgentRuntime, PrefixStatus, RuntimeEvent, RuntimeEventReceiver, ToolCallId, TurnId,
    TurnTelemetry,
};
pub use runtime_launch::{LaunchedRuntime, launch_runtime, web_enabled};
pub use sandbox::{sandbox_available, sandbox_confines_filesystem_and_network};
pub use session::Session;
pub use session_entry::{EntryKind, ExchangeResult, SessionEntry, ToolExchange};
pub use session_store::{
    CheckpointRecord, JsonSessionStore, SessionId, SessionRecord, SessionStore, SessionStoreError,
    TurnRecord, format_sessions_storage_note, now_ms,
};
pub use shell_tools::JobStore;
pub use subagent::{SharedSubAgentManager, SubAgentManager, is_subagent_tool};
pub use tool::{
    ApprovalDecision, ApprovalRequest, MockEchoTool, ToolError, ToolRegistry, ToolResult,
    ToolResultStatus,
};
pub use workspace_policy::WorkspaceRoots;
pub use workspace_summary::list_workspace_files;
