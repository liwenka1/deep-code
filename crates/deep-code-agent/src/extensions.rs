use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::handle::{HandleStore, register_handle_read};
use crate::rlm::{RlmServices, register_rlm_tools};
use crate::subagent::{
    SubAgentServices, register_subagent_tools, DEFAULT_MAX_CONCURRENT,
};
use crate::tool::ToolRegistry;

/// Shared parent-runtime services for handles, sub-agents, and RLM sessions.
pub struct AgentExtensions<C: LlmClient + Clone + 'static> {
    pub handle_store: Arc<RwLock<HandleStore>>,
    pub subagent: Arc<SubAgentServices<C>>,
    pub rlm: Arc<RlmServices>,
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
}

pub fn attach_agent_extensions<C: LlmClient + Clone + 'static>(
    registry: &mut ToolRegistry,
    client: Arc<C>,
    workspace: PathBuf,
    parent_cancel: CancellationToken,
) -> Arc<AgentExtensions<C>> {
    let handle_store = Arc::new(RwLock::new(HandleStore::new()));
    let exec_policy = registry.policy().clone();
    let subagent = Arc::new(SubAgentServices::new(
        client,
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
    })
}
