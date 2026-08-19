use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::execution_policy::ExecPolicy;
use crate::shell_tools::shell_tool_registry_from;
use crate::subagent::roles::{SubAgentRole, build_system_prompt};
use crate::tool::ToolRegistry;
use crate::workspace_policy::WorkspacePolicy;
#[cfg(test)]
use crate::workspace_policy::WorkspaceRoots;
use crate::workspace_tools::workspace_tool_registry_from;

use super::manager::SubAgentManager;
use super::tools::AgentTool;

pub const SUBAGENT_TOOL_NAMES: [&str; 1] = ["agent"];

pub fn is_subagent_tool(name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&name)
}

/// Shared services wired into parent and child agent runtimes.
pub struct SubAgentServices {
    pub manager: Arc<RwLock<SubAgentManager>>,
    pub client: Arc<dyn LlmClient>,
    pub agent_config: AgentConfig,
    /// The parent's live write boundary, shared (not snapshotted) by every
    /// child: a sub-agent works the same boundary as its parent — narrower
    /// would break delegated cross-root tasks, wider is not the dispatcher's
    /// to grant. Sharing the policy handle means a user-approved mid-session
    /// grant reaches children spawned afterwards (and running ones) exactly
    /// like it reaches the parent's own tools.
    pub(crate) boundary: WorkspacePolicy,
    pub parent_cancel: CancellationToken,
    pub exec_policy: ExecPolicy,
}

impl SubAgentServices {
    pub(crate) fn new(
        client: Arc<dyn LlmClient>,
        agent_config: AgentConfig,
        boundary: WorkspacePolicy,
        parent_cancel: CancellationToken,
        max_concurrent: usize,
        exec_policy: ExecPolicy,
    ) -> Self {
        let manager = Arc::new(RwLock::new(SubAgentManager::new(max_concurrent)));
        Self {
            manager,
            client,
            agent_config,
            boundary,
            parent_cancel,
            exec_policy,
        }
    }

    /// Cancel every child (all child tokens derive from `parent_cancel`) and
    /// mark running records cancelled.
    pub fn cancel_all_running(&self) {
        self.parent_cancel.cancel();
        if let Ok(mut manager) = self.manager.write() {
            manager.cancel_all();
        }
    }
}

pub fn register_subagent_tools(registry: &mut ToolRegistry, services: Arc<SubAgentServices>) {
    registry.register(AgentTool::new(services));
}

/// Attach sub-agent tools to an existing parent registry. Test-only: the
/// production path goes through [`crate::extensions::attach_agent_extensions`].
#[cfg(test)]
pub fn attach_subagent_tools(
    registry: &mut ToolRegistry,
    client: Arc<dyn LlmClient>,
    agent_config: AgentConfig,
    roots: impl Into<WorkspaceRoots>,
    parent_cancel: CancellationToken,
) -> Arc<SubAgentServices> {
    let boundary = WorkspacePolicy::new(roots).expect("test roots must resolve");
    Arc::clone(
        &crate::extensions::attach_agent_extensions(
            registry,
            client,
            agent_config,
            boundary,
            parent_cancel,
        )
        .subagent,
    )
}

/// Build a child tool registry filtered by role (no recursive sub-agent
/// tools, and no `request_write_root` — widening the boundary is a
/// parent-loop conversation with the human; a child's request would be
/// auto-denied anyway, see `subagent_approval_decision`).
pub(crate) fn child_tool_registry(
    boundary: &WorkspacePolicy,
    role: SubAgentRole,
    exec_policy: ExecPolicy,
) -> ToolRegistry {
    let workspace_tools = workspace_tool_registry_from(boundary.clone());
    let mut registry =
        ToolRegistry::filtered_from(&workspace_tools, |name| include_workspace_tool(role, name));
    registry.set_policy(exec_policy);
    // All roles may use the shell: child policy auto-denies anything unapproved,
    // so read-only roles effectively get only trusted read-only prefixes
    // (git status/diff/log, …).
    let (shell_tools, _) = shell_tool_registry_from(boundary.clone());
    registry.extend(shell_tools);
    registry
}

fn include_workspace_tool(role: SubAgentRole, name: &str) -> bool {
    match name {
        "write_file" | "apply_patch" => role.allows_writes(),
        _ => matches!(name, "read_file" | "list_dir" | "grep_files"),
    }
}

#[must_use]
pub fn child_system_prompt(role: SubAgentRole) -> String {
    build_system_prompt(role)
}
