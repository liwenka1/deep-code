//! Unified execution policy for tools and shell commands.
//!
//! Policy is independent of TUI: callers evaluate a [`ToolExecutionPlan`] before
//! running tools or spawning subprocesses.

pub mod command_shape;
mod engine;
mod permission_mode;
mod shell_deny;

pub use engine::{
    ExecPolicy, NetworkMode, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind,
    accept_edits_approvable, justification_claimed, network_requested,
};
pub use permission_mode::{PermissionMode, SharedPermissionMode};
pub use shell_deny::{SafetyNote, safety_notes};
