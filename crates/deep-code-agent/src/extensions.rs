use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::skills::build_system_prompt;
use crate::subagent::{DEFAULT_MAX_CONCURRENT, SubAgentServices, register_subagent_tools};
use crate::tool::ToolRegistry;

/// Shared parent-runtime services for sub-agents.
pub struct AgentExtensions {
    pub subagent: Arc<SubAgentServices>,
}

impl AgentExtensions {
    pub fn cancel_all_running(&self) {
        self.subagent.cancel_all_running();
    }

    #[must_use]
    pub fn subagent_manager(&self) -> Arc<RwLock<crate::subagent::SubAgentManager>> {
        Arc::clone(&self.subagent.manager)
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

/// Sub-agent delegation guidance for the parent runtime. The `agent` tool is
/// always mounted, but otherwise the model has only its one-line description —
/// without this it tends to never delegate, or to brief children so vaguely
/// they wander. Written for the blocking model: one `agent` call runs one
/// child to completion and returns its report, so parallelism is simply
/// "several `agent` calls in one turn," with nothing to poll or close.
pub const SUBAGENT_GUIDANCE: &str = "\
子代理委托 / Sub-agents（agent 工具）:\n\
- 何时派：调查、代码审查、验证类任务，且结论远小于过程——子代理把中间的读取/搜索开销烧在它自己的上下文里，父代理只收回精简报告。彼此独立、可并行的多路调查，在同一轮内发出多个 agent 调用即并发执行。\n\
- 何时不派：简单任务，或需要当前对话上下文的任务——子代理是全新上下文，会从零重新发现一切，反而更慢更贵。\n\
- 简报必须自足：子代理只看得到你给的 task 文本。写清四项——目标、范围边界、已知线索（相关文件/符号路径）、验收标准（报告需要回答什么）。简报越含糊，子代理越会跑偏。\n\
- 选角色 role：explore / review / verifier 只读（调查/评审/验证）；implementer / general 可写文件（落地改动）；拿不准用 general。\n\
- 一次调用即阻塞到子代理完成并返回其五段报告，无需轮询或关闭；子代理产出报告即停，不会反问。";

pub fn build_runtime_system_prompt(base: &str, workspace: &Path) -> String {
    let prompt = build_system_prompt(base, workspace);
    format!("{prompt}\n\n{TOOL_DISCIPLINE}\n\n{SUBAGENT_GUIDANCE}")
}

pub fn attach_agent_extensions(
    registry: &mut ToolRegistry,
    client: Arc<dyn LlmClient>,
    agent_config: AgentConfig,
    workspace: PathBuf,
    parent_cancel: CancellationToken,
) -> Arc<AgentExtensions> {
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
    Arc::new(AgentExtensions { subagent })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn runtime_prompt_includes_tool_discipline_and_subagent_guidance() {
        let dir = TempDir::new().unwrap();
        let prompt = build_runtime_system_prompt("base prompt", dir.path());
        assert!(prompt.starts_with("base prompt"));
        assert!(prompt.contains("工具使用纪律"));
        assert!(prompt.contains("不要反复读取同一文件"));
        // Sub-agent guidance rides the same parent-only path.
        assert!(prompt.contains("子代理委托"));
        assert!(prompt.contains("简报必须自足"));
    }
}
