//! Unified execution policy for tools and shell commands.
//!
//! Policy is independent of TUI: callers evaluate a [`ToolExecutionPlan`] before
//! running tools or spawning subprocesses.

mod engine;

pub use engine::{
    ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind, evaluate_shell_command,
};
