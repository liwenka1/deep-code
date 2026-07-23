//! Unified execution policy for tools and shell commands.
//!
//! Policy is independent of TUI: callers evaluate a [`ToolExecutionPlan`] before
//! running tools or spawning subprocesses.

pub mod command_shape;
mod engine;
mod shell_deny;

pub use engine::{ExecPolicy, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind};
pub use shell_deny::{SafetyNote, safety_notes};
