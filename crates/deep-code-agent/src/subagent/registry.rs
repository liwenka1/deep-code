use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::execution_policy::{ExecPolicy, SharedPermissionMode};
use crate::shell_tools::{JobStore, shell_tool_registry_from};
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
    /// The parent's live permission mode, shared rather than snapshotted: a
    /// child's gate then follows the session exactly as the parent's own tools
    /// do, so Shift+Tab mid-session reaches a child that is already running.
    ///
    /// A child inherits rather than being pinned to `Default`. The old pin was
    /// argued from "a child runs unattended, so its prompts can only be
    /// auto-decided" — which is true, and is exactly why pinning it stricter
    /// than its dispatcher strands work instead of protecting anything: a child
    /// could not run a command outside the trust list by any route, not even a
    /// `git push` with egress granted, because no approval path existed for it
    /// to take. Inheritance keeps the dispatcher's decision as the decision
    /// point (that is where the human saw the request) and lets the child act
    /// on it.
    pub permission_mode: SharedPermissionMode,
    pub exec_policy: ExecPolicy,
}

impl SubAgentServices {
    pub(crate) fn new(
        client: Arc<dyn LlmClient>,
        agent_config: AgentConfig,
        boundary: WorkspacePolicy,
        parent_cancel: CancellationToken,
        permission_mode: SharedPermissionMode,
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
            permission_mode,
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
            SharedPermissionMode::default(),
        )
        .subagent,
    )
}

/// Build a child tool registry filtered by role (no recursive sub-agent
/// tools, and no `request_write_root` — widening the boundary is a
/// parent-loop conversation with the human; a child's request would be
/// auto-denied anyway, see `subagent_approval_decision`).
///
/// `network` is the dispatch-time grant (the `agent` call declared
/// `network: true` and passed its approval gate — reaching execution IS the
/// consent, same as `role.allows_writes()`). A granted child gets the web
/// tools and its exec policy switches to ambient egress: the child runs
/// unattended, so per-command network declarations would protect nothing —
/// nobody is watching its prompts, they are auto-denied. The trusted-command
/// wall is separate and does not widen: network changes whether allow-listed
/// commands get egress, never which commands may run.
/// Builds the child registry AND returns its [`JobStore`], which the caller
/// MUST `shutdown()` when the child finalizes. The store is shared with the
/// registry's background-job tool, and its background jobs outlive the child's
/// turn — so, unlike the main agent (whose `RuntimeHandle` shuts the store down
/// on quit), a discarded child store never process-group-killed its jobs and
/// orphaned their grandchildren (dev servers, build trees) until process exit.
pub(crate) fn child_tool_registry(
    boundary: &WorkspacePolicy,
    role: SubAgentRole,
    exec_policy: ExecPolicy,
    network: bool,
    ui_lang: &crate::i18n::SharedLang,
) -> (ToolRegistry, JobStore) {
    let workspace_tools = workspace_tool_registry_from(boundary.clone());
    let mut registry =
        ToolRegistry::filtered_from(&workspace_tools, |name| include_workspace_tool(role, name));
    let exec_policy = if network {
        exec_policy.with_network_mode(crate::execution_policy::NetworkMode::Always)
    } else {
        // A child dispatched WITHOUT network:true must not inherit ambient
        // `Always` egress from a global `[sandbox] network = "always"`: that
        // would hand its allow-listed commands egress no human approved for this
        // child (the dispatch-approval invariant), while its system prompt tells
        // it it has no network at all. Cap at `Prompt` so ambient egress is only
        // reachable through a network:true dispatch — itself auto-denied for an
        // unattended child, i.e. no egress. Never widen a stricter parent:
        // `never` stays `never` (rank keeps the tighten-only direction).
        use crate::execution_policy::NetworkMode;
        let capped = if exec_policy.network_mode().rank() > NetworkMode::Prompt.rank() {
            NetworkMode::Prompt
        } else {
            exec_policy.network_mode()
        };
        exec_policy.with_network_mode(capped)
    };
    // Read before the move: the shell registry's own description and manager
    // have to agree with the policy that gates the calls, and this is the last
    // point where both are in hand.
    let unconfined = exec_policy.sandbox_off();
    registry.set_policy(exec_policy);
    // All roles may use the shell: child policy auto-denies anything unapproved,
    // so read-only roles effectively get only trusted read-only prefixes
    // (git status/diff/log, …).
    let (shell_tools, job_store) = shell_tool_registry_from(boundary.clone(), unconfined);
    registry.extend(shell_tools);
    if network {
        // The web tools ride the same grant: in-process fetch/search behind
        // the SSRF pin, the research half of "this child may reach the
        // network". Absent a grant they are absent, not merely gated — an
        // unattended prompt could only be auto-denied anyway.
        registry.extend(crate::web_tools::web_tool_registry(ui_lang));
    }
    (registry, job_store)
}

fn include_workspace_tool(role: SubAgentRole, name: &str) -> bool {
    match name {
        "write_file" | "apply_patch" => role.allows_writes(),
        _ => matches!(name, "read_file" | "list_dir" | "grep_files"),
    }
}

#[must_use]
pub fn child_system_prompt(role: SubAgentRole, network: bool) -> String {
    build_system_prompt(role, network)
}
