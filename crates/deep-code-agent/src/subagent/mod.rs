//! Persistent sub-agent sessions for parallel delegated work.

mod manager;
mod output;
mod registry;
mod roles;
mod runner;
mod tools;
mod types;

#[cfg(test)]
mod tests;

pub use manager::SubAgentManager;
pub use registry::{
    SubAgentServices, attach_subagent_tools, is_subagent_tool, register_subagent_tools,
    subagent_tool_registry,
};
pub use roles::SubAgentRole;
pub use tools::{AgentCloseTool, AgentEvalTool, AgentOpenTool};
pub use types::{
    DEFAULT_MAX_CONCURRENT, HARD_MAX_CONCURRENT, SubAgentRecord, SubAgentSessionProjection,
    SubAgentStatus, StructuredReport,
};

pub type SharedSubAgentManager = std::sync::Arc<std::sync::RwLock<SubAgentManager>>;
