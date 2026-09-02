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
//! 1. **Plan** — [`ExecPolicy::evaluate_tool`] → [`ToolExecutionPlan`]. A pure,
//!    stateless function of `(tool, args)`: the verdict, risk tier, sandbox and
//!    trust-match. Knows nothing of the session, the mode, or standing consent.
//!    For `shell` and `job action=start` its first step is the **deny floor**,
//!    `shell_deny::builtin_deny` (via `shell_lex` parsing): a catastrophic shape
//!    gets a `Deny` verdict before any trust rule is consulted, so nothing can
//!    allow-list past it. Cannot be disabled by configuration.
//! 2. **Yolo egress overlay** — `runtime::tool_result::yolo_ambient_network`.
//!    The one post-hoc edit to the plan: under `Yolo`, ambient network rides a
//!    sandboxed plan. It never touches the verdict. (`[sandbox] network =
//!    "never"` still wins; see [`NetworkMode`].)
//! 3. **Deny short-circuit** —
//!    [`crate::tool::ToolRegistry::run_tool_call_with_plan`] returns the policy
//!    error for a plan whose [`ToolExecutionPlan::denied_reason`] is `Some`
//!    before consulting approval at all (the sub-agent decision path in
//!    `runtime.rs` checks the same). A hard denial therefore never reaches
//!    consent, the mode, or the judge.
//! 4. **Standing consent** — `runtime::approval_flow` (config `auto_allow` +
//!    session "approve for the session", matched by command identity via
//!    [`command_shape`]).
//! 5. **Permission mode** — `runtime::approval_flow`, keyed on
//!    [`PermissionMode`]: `Default` asks, `AcceptEdits` waves through in-workspace
//!    edits ([`accept_edits_approvable`]), `Auto` consults the judge below,
//!    `Yolo` waves through all but a root grant.
//! 6. **Auto judge** — the cheap classifier. It only ever sees a call that has
//!    already cleared three gates it cannot override: a root grant
//!    (`request_write_root`) asks a human in every mode, egress (a network-native
//!    tool or a declared `network`) is decided before the judge is consulted, and
//!    the top risk tier always asks. The judge's own fail-safes — an offline
//!    backend cannot judge, a cancel mid-flight aborts into "ask" — are
//!    documented on `auto_mode_approves` in `runtime::approval_flow`.
//!
//! Only stage 1 can say an automatic hard "no" (enforced at stage 3). Every
//! stage below can only relax a `NeedsApproval` into running — one from stage 1
//! may still be auto-approved by stage 4, 5 or 6 — never tighten an `Allow`.
//! Reading one stage in isolation is therefore misleading: follow the whole
//! chain.

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
