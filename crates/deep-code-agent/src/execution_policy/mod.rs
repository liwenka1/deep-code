//! Unified execution policy for tools and shell commands.
//!
//! Policy is independent of TUI: callers evaluate a [`ToolExecutionPlan`] before
//! running tools or spawning subprocesses.
//!
//! # The gate, end to end
//!
//! "May this tool call run, and does it need a human first?" is answered by an
//! ordered pipeline that spans this module and the runtime. It lives in several
//! files on purpose — pure policy here, stateful resolution in the runtime — so
//! this map is the one place that names the stages and where each lives:
//!
//! 1. **Deny floor** — `shell_deny::builtin_deny` (via `shell_lex` parsing).
//!    Catastrophic shell shapes are hard-refused; short-circuits in the tool
//!    registry *before* any decision below runs, so nothing can allow-list past
//!    it. Cannot be disabled by configuration.
//! 2. **Plan** — [`ExecPolicy::evaluate_tool`] → [`ToolExecutionPlan`]. A pure,
//!    stateless function of `(tool, args)`: the verdict, risk tier, sandbox and
//!    trust-match. Knows nothing of the session, the mode, or standing consent.
//! 3. **Yolo egress overlay** — `runtime::tool_result::yolo_ambient_network`.
//!    The one post-hoc edit to the plan: under `Yolo`, ambient network rides the
//!    plan. (`[sandbox] network = "never"` still wins; see [`NetworkMode`].)
//! 4. **Standing consent** — `runtime::approval_flow` (config `auto_allow` +
//!    session "approve for the session", matched by command identity via
//!    [`command_shape`]).
//! 5. **Permission mode** — `runtime::approval_flow`, keyed on
//!    [`PermissionMode`]: `Default` asks, `AcceptEdits` waves through in-workspace
//!    edits ([`accept_edits_approvable`]), `Auto` consults the judge below,
//!    `Yolo` waves through all but a root grant.
//! 6. **Auto judge** — the cheap classifier, *below three hard floors it can
//!    never override*: the top risk tier, any network-native tool or declared
//!    egress, and `request_write_root` always ask a human.
//!
//! Reading one stage in isolation is misleading: a call `Allow`ed by stage 2 can
//! still be parked by stage 5, and a `NeedsApproval` from stage 2 can still be
//! auto-approved by stage 4. Follow the whole chain.

pub mod command_shape;
mod engine;
mod permission_mode;
mod shell_deny;
mod shell_lex;

pub use engine::{
    ExecPolicy, NetworkMode, PolicyVerdict, RiskLevel, ToolExecutionPlan, ToolKind,
    accept_edits_approvable, justification_claimed, network_requested, shell_command_of,
};
pub use permission_mode::{PermissionMode, SharedPermissionMode};
pub use shell_deny::{SafetyNote, safety_notes};
