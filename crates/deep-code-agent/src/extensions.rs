use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::handle::{HandleStore, register_handle_read};
use crate::hooks::{HookDispatcher, HooksConfig, load_hooks_config};
use crate::mcp::{McpManager, register_mcp_tools};
use crate::rlm::{RlmServices, register_rlm_tools};
use crate::skills::build_system_prompt;
use crate::subagent::{DEFAULT_MAX_CONCURRENT, SubAgentServices, register_subagent_tools};
use crate::tool::ToolRegistry;

/// Shared parent-runtime services for handles, sub-agents, RLM, and MCP.
pub struct AgentExtensions<C: LlmClient + Clone + 'static> {
    pub handle_store: Arc<RwLock<HandleStore>>,
    pub subagent: Arc<SubAgentServices<C>>,
    pub rlm: Arc<RlmServices>,
    pub mcp: Arc<RwLock<McpManager>>,
    pub hooks: Arc<HookDispatcher>,
}

impl<C: LlmClient + Clone + 'static> AgentExtensions<C> {
    pub fn cancel_all_running(&self) {
        self.subagent.cancel_all_running();
        if let Ok(mut manager) = self.rlm.manager.write() {
            manager.close_all();
        }
    }

    #[must_use]
    pub fn subagent_manager(&self) -> Arc<RwLock<crate::subagent::SubAgentManager>> {
        Arc::clone(&self.subagent.manager)
    }

    pub fn reload_mcp(&self, workspace: &Path) -> Result<(), crate::mcp::McpError> {
        let manager = McpManager::load_from_workspace(workspace)?;
        *self.mcp.write().expect("mcp lock") = manager;
        Ok(())
    }
}

pub struct RuntimeBootstrap {
    pub hooks: Arc<HookDispatcher>,
    pub mcp: Arc<RwLock<McpManager>>,
}

impl RuntimeBootstrap {
    pub fn load(workspace: &Path, hooks_config: Option<HooksConfig>) -> Self {
        let hooks = Arc::new(HookDispatcher::from_config(
            &hooks_config.unwrap_or_else(load_hooks_config),
        ));
        let mcp = Arc::new(RwLock::new(
            McpManager::load_from_workspace(workspace).unwrap_or_default(),
        ));
        Self { hooks, mcp }
    }
}

pub fn build_runtime_system_prompt(base: &str, workspace: &Path) -> String {
    build_system_prompt(base, workspace)
}

pub fn attach_runtime_tools(registry: &mut ToolRegistry, bootstrap: &RuntimeBootstrap) {
    registry.set_hooks(Arc::clone(&bootstrap.hooks));
    register_mcp_tools(registry, Arc::clone(&bootstrap.mcp));
}

pub fn attach_agent_extensions<C: LlmClient + Clone + 'static>(
    registry: &mut ToolRegistry,
    client: Arc<C>,
    agent_config: AgentConfig,
    workspace: PathBuf,
    parent_cancel: CancellationToken,
    bootstrap: &RuntimeBootstrap,
) -> Arc<AgentExtensions<C>> {
    attach_runtime_tools(registry, bootstrap);
    let handle_store = Arc::new(RwLock::new(HandleStore::new()));
    let exec_policy = registry.policy().clone();
    let subagent = Arc::new(SubAgentServices::new(
        client,
        agent_config,
        workspace.clone(),
        parent_cancel,
        DEFAULT_MAX_CONCURRENT,
        exec_policy,
        Arc::clone(&handle_store),
    ));
    register_subagent_tools(registry, Arc::clone(&subagent));
    register_handle_read(registry, Arc::clone(&handle_store));
    let rlm = Arc::new(RlmServices::new(Arc::clone(&handle_store), workspace));
    register_rlm_tools(registry, Arc::clone(&rlm));
    Arc::new(AgentExtensions {
        handle_store,
        subagent,
        rlm,
        mcp: Arc::clone(&bootstrap.mcp),
        hooks: Arc::clone(&bootstrap.hooks),
    })
}
