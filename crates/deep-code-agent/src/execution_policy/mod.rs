//! Unified execution policy for tools and shell commands.
//!
//! Policy is independent of TUI: callers evaluate a [`ToolExecutionPlan`] before
//! running tools or spawning subprocesses.

pub mod bash_arity;
mod engine;
mod shell_deny;

pub use engine::{
    ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind, evaluate_shell_command,
};
pub use shell_deny::{SafetyNotes, safety_notes};
