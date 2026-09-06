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
//!    stateless function of `(tool, args)` and the policy's own configuration
//!    (trust list, sandbox flag, network mode): the verdict, risk tier, sandbox
//!    and trust-match. Knows nothing of the session, the mode, or standing
//!    consent.
//!    For `shell` and `job action=start` its first step is the **deny floor**,
//!    `shell_deny::builtin_deny` (via `shell_lex` parsing): a catastrophic shape
//!    gets a `Deny` verdict before any trust rule is consulted, so nothing can
//!    allow-list past it. Cannot be disabled by configuration. The plan's one
//!    *configuration-driven* `Deny` sits right after it: a `network: true`
//!    declaration (shell, `job start`, or a sub-agent dispatch) under
//!    `[sandbox] network = "never"` is refused outright rather than run
//!    offline to fail.
//! 2. **Yolo egress overlay** — `runtime::tool_result::yolo_ambient_network`.
//!    The one post-hoc edit to the plan: under `Yolo`, ambient network rides a
//!    sandboxed plan. It never touches the verdict. (`[sandbox] network =
//!    "never"` still wins; see [`NetworkMode`].)
//! 3. **Deny short-circuit** —
//!    [`crate::tool::ToolRegistry::run_tool_call_with_plan`] returns the policy
//!    error for a plan whose [`ToolExecutionPlan::denied_reason`] is `Some`
//!    before consulting approval at all (the sub-agent decision path in
//!    `runtime.rs` checks the same). A hard denial therefore never reaches
//!    consent, the mode, or the judge. The registry then asks whenever the plan
//!    says so *or* the tool's own spec declares `requires_approval`; every tool
//!    that declares it (the write tools, the root grant, the mock) already gets
//!    a `NeedsApproval` plan, so that flag is belt-and-braces, not a second
//!    policy.
//! 4. **Standing consent** — `runtime::approval_flow`: config `auto_allow` (the
//!    user's explicit list, matched on the exact tool name) and session
//!    "approve for the session". Session memory is by tool name too, except for
//!    shell and `job action=start`, where it is remembered at command-identity
//!    granularity in one shared set ([`command_shape::session_identity`]); a
//!    job control action (status/tail/cancel), a sub-agent dispatch or a
//!    compound command records no session consent at all
//!    (`session_consent_recordable`). One exclusion sits *above* both consents:
//!    `request_write_root` is never covered by `auto_allow` or session memory
//!    (`auto_approval_granted` refuses it before consulting either), so no
//!    standing consent can pre-approve a boundary widening.
//! 5. **Permission mode** — `runtime::approval_flow`, keyed on
//!    [`PermissionMode`]: `Default` asks, `AcceptEdits` waves through workspace
//!    file edits, the dispatch of a writing sub-agent and filesystem-shaped
//!    shell commands ([`accept_edits_approvable`] — by program name; the
//!    sandbox, not this check, bounds their paths), `Auto` inherits that
//!    AcceptEdits allowance and consults the judge below for the rest, `Yolo`
//!    waves through all but a root grant.
//! 6. **Auto judge** — the cheap classifier. It only ever sees a call that has
//!    already cleared three gates it cannot override: a root grant
//!    (`request_write_root`) is never auto-approved in any mode — it asks a
//!    human, or is refused before the prompt (below), egress (a network-native
//!    tool or a declared `network`) is decided before the judge is consulted, and
//!    the top risk tier never reaches it: a High-tier call asks unless the
//!    inherited AcceptEdits allowance already covers it (an untrusted `mkdir
//!    src/x` is High by default and runs without a prompt in Auto exactly as it
//!    does in AcceptEdits). The judge's own fail-safes — an offline backend
//!    cannot judge, a cancel mid-flight aborts into "ask" — are documented on
//!    `auto_mode_approves` in `runtime::approval_flow`.
//!
//! Only stage 1 can say an automatic hard "no" (enforced at stage 3). Every
//! stage below can only relax a `NeedsApproval` into running — one from stage 1
//! may still be auto-approved by stage 4, 5 or 6 — never tighten an `Allow`.
//! Reading one stage in isolation is therefore misleading: follow the whole
//! chain.
//!
//! One more automatic refusal lives in the runtime and is not a policy verdict
//! at all (consumers add their own on top — headless `-p` auto-denies every
//! prompt, a child runtime decides its prompts in `subagent_approval_decision`):
//! when a `request_write_root` is about to be parked, the runtime resolves its
//! target once (`root_grant_prompt_target` in `runtime::approval_flow`) and
//! bounces an unresolvable or categorically refused one — the filesystem root,
//! the home directory, a credential store, a non-directory — straight back to
//! the model without prompting, because no human answer could make that grant
//! performable.

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
