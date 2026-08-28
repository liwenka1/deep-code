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
pub use registry::{SubAgentServices, is_subagent_tool, register_subagent_tools};
// Crate-internal: `runtime_launch`'s classify_tool test names the late-mounted
// sub-agent tool explicitly, because `build_parent_tools` does not mount it.
#[cfg(test)]
pub(crate) use registry::SUBAGENT_TOOL_NAMES;
pub use roles::SubAgentRole;
pub use types::DEFAULT_MAX_CONCURRENT;

pub type SharedSubAgentManager = std::sync::Arc<std::sync::RwLock<SubAgentManager>>;
