//! Plan mode: a read-only investigation mode enforced through a
//! [`ToolInterceptor`](crate::hooks::ToolInterceptor).
//!
//! When active, only strictly read-only tools run; any mutating or
//! side-effecting tool is blocked with a reason the model reads, so it keeps
//! planning instead of acting. This rides the interceptor seam rather than a
//! bespoke gate, and is entirely separate from approval (the execution policy):
//! plan mode is an extra, user-toggled ceiling, not a permission decision.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::execution_policy::{ExecPolicy, ToolKind};
use crate::hooks::{ToolGate, ToolInterceptor};
use crate::tool::ToolCall;

/// Shared, cheaply-cloneable plan-mode switch. Clones observe the same flag, so
/// the TUI toggle and the interceptor stay in lockstep without a channel.
#[derive(Debug, Clone, Default)]
pub struct PlanMode(Arc<AtomicBool>);

impl PlanMode {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn active(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }

    /// Flip the switch and return the new state.
    pub fn toggle(&self) -> bool {
        // fetch_xor returns the previous value; the new state is its negation.
        !self.0.fetch_xor(true, Ordering::SeqCst)
    }
}

/// Enforces [`PlanMode`]: read-only tools pass, everything else is blocked
/// while plan mode is on.
pub struct PlanModeInterceptor {
    plan_mode: PlanMode,
}

impl PlanModeInterceptor {
    #[must_use]
    pub fn new(plan_mode: PlanMode) -> Self {
        Self { plan_mode }
    }

    /// Tools permitted in plan mode: strictly read-only, no side effects. This
    /// is deliberately narrower than the policy's read-only set — sub-agents,
    /// MCP, and RLM-eval are blocked too, since a child or external tool could
    /// mutate state the interceptor can't see.
    fn is_read_only(name: &str) -> bool {
        matches!(
            ExecPolicy::classify_tool(name),
            ToolKind::ReadOnlyFile | ToolKind::Search | ToolKind::HandleRead
        )
    }
}

impl ToolInterceptor for PlanModeInterceptor {
    fn before_tool(&self, call: &ToolCall) -> ToolGate {
        if self.plan_mode.active() && !Self::is_read_only(&call.name) {
            ToolGate::Block {
                reason: format!(
                    "计划模式(只读): `{}` 会修改文件或产生副作用,已拦截。请先给出计划,\
                     退出计划模式(/plan)后再执行。",
                    call.name
                ),
            }
        } else {
            ToolGate::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall::new("c1", name, json!({}))
    }

    #[test]
    fn toggle_flips_and_reports_new_state() {
        let plan = PlanMode::new();
        assert!(!plan.active());
        assert!(plan.toggle());
        assert!(plan.active());
        assert!(!plan.toggle());
        assert!(!plan.active());
    }

    #[test]
    fn inactive_plan_mode_allows_everything() {
        let interceptor = PlanModeInterceptor::new(PlanMode::new());
        assert_eq!(
            interceptor.before_tool(&call("write_file")),
            ToolGate::Allow
        );
        assert_eq!(interceptor.before_tool(&call("shell")), ToolGate::Allow);
    }

    #[test]
    fn active_plan_mode_blocks_writes_allows_reads() {
        let plan = PlanMode::new();
        plan.set(true);
        let interceptor = PlanModeInterceptor::new(plan);
        // Read-only tools pass.
        assert_eq!(interceptor.before_tool(&call("read_file")), ToolGate::Allow);
        assert_eq!(
            interceptor.before_tool(&call("grep_files")),
            ToolGate::Allow
        );
        assert_eq!(
            interceptor.before_tool(&call("handle_read")),
            ToolGate::Allow
        );
        // Mutating / side-effecting tools are blocked.
        for name in ["write_file", "apply_patch", "shell", "job", "web_search"] {
            assert!(
                matches!(interceptor.before_tool(&call(name)), ToolGate::Block { .. }),
                "{name} must be blocked in plan mode"
            );
        }
    }
}
