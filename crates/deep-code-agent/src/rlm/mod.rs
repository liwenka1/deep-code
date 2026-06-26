//! Minimal persistent analysis sessions (RLM v1).
//!
//! v1 uses a bounded command runtime over loaded context instead of a full
//! Python REPL. The tool surface is backend-agnostic so richer backends can
//! be swapped in later.

mod runtime;
mod session;
mod tools;

#[cfg(test)]
mod tests;

pub use session::{RlmConfig, RlmManager, RlmSessionInfo};
pub use tools::{
    RLM_TOOL_NAMES, RlmCloseTool, RlmConfigureTool, RlmEvalTool, RlmOpenTool, RlmServices,
    is_rlm_tool, register_rlm_tools,
};
