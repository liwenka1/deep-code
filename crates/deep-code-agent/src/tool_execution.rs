//! Thread-local execution plan for the current tool invocation.
//!
//! Shell tools read the active plan to choose sandbox policy; set by
//! [`crate::tool::ToolRegistry::run_tool_call_with_plan`].

use std::cell::RefCell;

use crate::execution_policy::ToolExecutionPlan;
use crate::sandbox::SandboxPolicy;

thread_local! {
    static ACTIVE_PLAN: RefCell<Option<ToolExecutionPlan>> = const { RefCell::new(None) };
}

/// Run `action` while the active execution plan is set.
pub fn with_plan<R>(plan: ToolExecutionPlan, action: impl FnOnce() -> R) -> R {
    ACTIVE_PLAN.with(|cell| {
        *cell.borrow_mut() = Some(plan);
        let output = action();
        *cell.borrow_mut() = None;
        output
    })
}

/// Sandbox policy for the current tool invocation, if any.
#[must_use]
pub fn current_sandbox_policy() -> SandboxPolicy {
    ACTIVE_PLAN.with(|cell| SandboxPolicy::from_execution_plan(cell.borrow().as_ref()))
}
