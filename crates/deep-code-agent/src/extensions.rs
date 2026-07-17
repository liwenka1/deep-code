use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::hooks::{HookDispatcher, HooksConfig, load_hooks_config};
use crate::plan_mode::{PlanMode, PlanModeInterceptor};
use crate::skills::build_system_prompt;
use crate::subagent::{DEFAULT_MAX_CONCURRENT, SubAgentServices, register_subagent_tools};
use crate::tool::ToolRegistry;

/// Shared parent-runtime services for sub-agents.
pub struct AgentExtensions<C: LlmClient + Clone + 'static> {
    pub subagent: Arc<SubAgentServices<C>>,
    pub hooks: Arc<HookDispatcher>,
    pub plan_mode: PlanMode,
}

impl<C: LlmClient + Clone + 'static> AgentExtensions<C> {
    pub fn cancel_all_running(&self) {
        self.subagent.cancel_all_running();
    }

    #[must_use]
    pub fn subagent_manager(&self) -> Arc<RwLock<crate::subagent::SubAgentManager>> {
        Arc::clone(&self.subagent.manager)
    }

    #[must_use]
    pub fn plan_mode(&self) -> PlanMode {
        self.plan_mode.clone()
    }
}

pub struct RuntimeBootstrap {
    pub hooks: Arc<HookDispatcher>,
    pub plan_mode: PlanMode,
}

impl RuntimeBootstrap {
    pub fn load(hooks_config: Option<HooksConfig>) -> Self {
        let mut dispatcher =
            HookDispatcher::from_config(&hooks_config.unwrap_or_else(load_hooks_config));
        // Plan mode rides the interceptor seam: register its gate now, while the
        // dispatcher is still mutable (Arc-wrapping freezes it). The shared
        // switch is surfaced so the TUI can toggle it.
        let plan_mode = PlanMode::new();
        dispatcher.add_interceptor(Arc::new(PlanModeInterceptor::new(plan_mode.clone())));
        let hooks = Arc::new(dispatcher);
        Self { hooks, plan_mode }
    }
}

/// Tool-use discipline appended to every runtime system prompt. Without it,
/// DeepSeek tends to micro-step (edit → grep → edit), flooding the
/// transcript with tiny tool calls.
pub const TOOL_DISCIPLINE: &str = "\
工具使用纪律 / Tool discipline:\n\
- 先规划再行动：想清楚需要哪些信息，再决定调用哪些工具。\n\
- 一次性读取所需文件，不要反复读取同一文件；能从已有上下文推断的内容不要再调工具确认。\n\
- 调查类调用尽量批量进行；拿到足够信息后直接给出结论或一次性完成修改，避免一步一调的碎步操作。\n\
- 工具结果已在上下文中，无需重复获取。";

pub fn build_runtime_system_prompt(base: &str, workspace: &Path) -> String {
    let prompt = build_system_prompt(base, workspace);
    format!("{prompt}\n\n{TOOL_DISCIPLINE}")
}

pub fn attach_runtime_tools(registry: &mut ToolRegistry, bootstrap: &RuntimeBootstrap) {
    registry.set_hooks(Arc::clone(&bootstrap.hooks));
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
    let exec_policy = registry.policy().clone();
    let subagent = Arc::new(SubAgentServices::new(
        client,
        agent_config,
        workspace.clone(),
        parent_cancel,
        DEFAULT_MAX_CONCURRENT,
        exec_policy,
    ));
    // Capability tiers: L1 = unconditional core, L2 = kept but gated,
    // L3 = reducible to core primitives (slated for removal/reduction).
    // L2: sub-agents are a deliberate capability; kept.
    register_subagent_tools(registry, Arc::clone(&subagent));
    Arc::new(AgentExtensions {
        subagent,
        hooks: Arc::clone(&bootstrap.hooks),
        plan_mode: bootstrap.plan_mode.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn runtime_prompt_includes_tool_discipline() {
        let dir = TempDir::new().unwrap();
        let prompt = build_runtime_system_prompt("base prompt", dir.path());
        assert!(prompt.starts_with("base prompt"));
        assert!(prompt.contains("工具使用纪律"));
        assert!(prompt.contains("不要反复读取同一文件"));
    }
}
