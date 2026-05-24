use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::execution_policy::ExecPolicy;
use crate::git_tools::git_tool_registry;
use crate::handle::HandleStore;
use crate::shell_tools::shell_tool_registry;
use crate::subagent::roles::{SubAgentRole, build_system_prompt};
use crate::tool::ToolRegistry;
use crate::workspace_tools::workspace_tool_registry;

use super::manager::SubAgentManager;
use super::tools::{AgentCloseTool, AgentEvalTool, AgentOpenTool};

pub type AgentCancelMap = Arc<RwLock<HashMap<String, CancellationToken>>>;

pub const SUBAGENT_TOOL_NAMES: [&str; 3] = ["agent_open", "agent_eval", "agent_close"];

pub fn is_subagent_tool(name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&name)
}

/// Shared services wired into parent and child agent runtimes.
pub struct SubAgentServices<C: LlmClient + Clone + 'static> {
    pub manager: Arc<RwLock<SubAgentManager>>,
    pub client: Arc<C>,
    pub workspace: PathBuf,
    pub parent_cancel: CancellationToken,
    pub handle_store: Arc<RwLock<HandleStore>>,
    pub agent_cancels: AgentCancelMap,
    pub exec_policy: ExecPolicy,
}

impl<C: LlmClient + Clone + 'static> SubAgentServices<C> {
    pub fn new(
        client: Arc<C>,
        workspace: PathBuf,
        parent_cancel: CancellationToken,
        max_concurrent: usize,
        exec_policy: ExecPolicy,
        handle_store: Arc<RwLock<HandleStore>>,
    ) -> Self {
        let manager = Arc::new(RwLock::new(SubAgentManager::new(
            workspace.clone(),
            max_concurrent,
            Arc::clone(&handle_store),
        )));
        Self {
            manager,
            client,
            workspace,
            parent_cancel,
            handle_store,
            agent_cancels: Arc::new(RwLock::new(HashMap::new())),
            exec_policy,
        }
    }

    /// Cancel parent + per-agent tokens and mark running records cancelled.
    pub fn cancel_all_running(&self) {
        self.parent_cancel.cancel();
        if let Ok(cancels) = self.agent_cancels.read() {
            for token in cancels.values() {
                token.cancel();
            }
        }
        if let Ok(mut manager) = self.manager.write() {
            manager.cancel_all();
        }
    }
}

pub fn register_subagent_tools<C: LlmClient + Clone + 'static>(
    registry: &mut ToolRegistry,
    services: Arc<SubAgentServices<C>>,
) {
    registry.register(AgentOpenTool::new(Arc::clone(&services)));
    registry.register(AgentEvalTool::new(Arc::clone(&services)));
    registry.register(AgentCloseTool::new(services));
}

/// Attach sub-agent tools to an existing parent registry.
pub fn attach_subagent_tools<C: LlmClient + Clone + 'static>(
    registry: &mut ToolRegistry,
    client: Arc<C>,
    workspace: PathBuf,
    parent_cancel: CancellationToken,
) -> Arc<SubAgentServices<C>> {
    Arc::clone(&crate::extensions::attach_agent_extensions(
        registry,
        client,
        workspace,
        parent_cancel,
    )
    .subagent)
}

pub fn subagent_tool_registry<C: LlmClient + Clone + 'static>(
    client: Arc<C>,
    workspace: PathBuf,
    parent_cancel: CancellationToken,
) -> (ToolRegistry, Arc<SubAgentServices<C>>) {
    let handle_store = Arc::new(RwLock::new(HandleStore::new()));
    let services = Arc::new(SubAgentServices::new(
        client,
        workspace,
        parent_cancel,
        super::types::DEFAULT_MAX_CONCURRENT,
        ExecPolicy::default(),
        handle_store,
    ));
    let mut registry = ToolRegistry::new();
    register_subagent_tools(&mut registry, Arc::clone(&services));
    (registry, services)
}

/// Build a child tool registry filtered by role (no recursive sub-agent tools).
pub fn child_tool_registry(
    workspace: &PathBuf,
    role: SubAgentRole,
    exec_policy: ExecPolicy,
) -> Result<ToolRegistry, crate::tool::ToolError> {
    let workspace_tools = workspace_tool_registry(workspace)?;
    let mut registry = ToolRegistry::filtered_from(&workspace_tools, |name| {
        include_workspace_tool(role, name)
    });
    registry.set_policy(exec_policy);
    if role.allows_shell() {
        registry.extend(shell_tool_registry(workspace)?);
    }
    registry.extend(git_tool_registry(workspace)?);
    Ok(registry)
}

fn include_workspace_tool(role: SubAgentRole, name: &str) -> bool {
    match name {
        "write_file" | "apply_patch" => role.allows_writes(),
        _ => matches!(
            name,
            "read_file" | "list_dir" | "grep_files" | "write_file" | "apply_patch"
        ),
    }
}

#[must_use]
pub fn child_system_prompt(role: SubAgentRole) -> String {
    build_system_prompt(role)
}
