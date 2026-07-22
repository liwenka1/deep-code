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
pub use roles::SubAgentRole;
pub use types::DEFAULT_MAX_CONCURRENT;

pub type SharedSubAgentManager = std::sync::Arc<std::sync::RwLock<SubAgentManager>>;
