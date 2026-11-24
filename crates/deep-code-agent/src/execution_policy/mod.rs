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
//!    (trust list, network mode): the verdict, risk tier, sandbox and
//!    trust-match. Knows nothing of the session, the mode, or standing
//!    consent.
//!    For `shell` and `job action=start` its first step is the **deny floor**,
//!    `shell_deny::builtin_deny` (via `shell_lex` parsing): a catastrophic shape
//!    gets a `Deny` verdict before any trust rule is consulted, so nothing can
//!    allow-list past it. Under `[sandbox] network = "never"`, the policy also
//!    denies commands and dispatches that declare `network: true`, as well as
//!    the network tools `fetch_url` and `web_search`.
//!
//!    The third floor is not a `Deny` but a park: an **irreversible-outward
//!    command** — publishing, force-pushing, merging a PR, applying
//!    infrastructure — that no `[sandbox] allow_irreversible` entry authorizes
//!    comes back as `NeedsApproval` carrying `ApprovalAuthority::HumanOnly`.
//!    This requires an explicit decision for the request, just like a write-root
//!    grant. Unattended consumers such as headless runs and sub-agents deny it
//!    with a note explaining the requirement. See `SECURITY.md`.
//! 2. **Yolo egress overlay** — `runtime::tool_result::yolo_ambient_network`.
//!    The one post-hoc edit to the plan: under `Yolo`, ambient network rides a
//!    sandboxed plan. It never touches the verdict. (`[sandbox] network =
//!    "never"` still wins; see [`NetworkMode`].)
//! 3. **Deny short-circuit** —
//!    [`crate::tool::ToolRegistry::run_tool_call_with_plan`] returns the policy
//!    error for a plan whose [`ToolExecutionPlan::denied_reason`] is `Some`
//!    before consulting approval at all (the sub-agent decision path in
//!    `runtime.rs` checks the same). A hard denial therefore never reaches
//!    consent or the mode. Tool registration's approval requirement is folded
//!    into the plan during evaluation. The verdict then drives approval. The
//!    registry parses typed tool parameters first and returns an error for
//!    malformed arguments.
//! 4. **Standing consent** — `runtime::approval_flow`: config `auto_allow` (the
//!    user's explicit list, matched on the exact tool name) and session
//!    "approve for the session". Session memory is by tool name too, except for
//!    shell and `job action=start`, where it is remembered at command-identity
//!    granularity in one shared set ([`command_shape::session_identity`]); a
//!    job control action (status/tail/cancel), a sub-agent dispatch or a
//!    compound command records no session consent at all
//!    (`session_consent_recordable`, which also refuses an irreversible command:
//!    a recorded identity for one could never be read). Two exclusions sit
//!    *above* both consents: `request_write_root` and a `HumanOnly` plan are
//!    never covered by `auto_allow` or session memory (`auto_approval_granted`
//!    refuses them before consulting either), so no standing consent can
//!    pre-approve a boundary widening or an irreversible command.
//! 5. **Permission mode** — `runtime::approval_flow`, keyed on
//!    [`PermissionMode`]: `Default` asks, `AcceptEdits` waves through workspace
//!    file edits, the dispatch of a writing sub-agent and filesystem-shaped
//!    shell commands ([`accept_edits_approvable`] — by program name, operands
//!    spelled relative and in-tree, with sandboxed writes). `Yolo` approves
//!    the remaining requests eligible for automatic approval.
//!
//! What runs once the chain says yes depends on WHO said it
//! (`crate::tool::RunAuthority`): a call the policy's own parse let through —
//! a trust-list `Allow`, a remembered session identity, the accept-edits
//! allowance — is executed as the argv `shell_lex::parse_unattended` produced,
//! with no shell in between. A call approved as text goes verbatim to
//! `sh -c` or `cmd /C` on Windows.
//!
//! # What is behind a command nobody read
//!
//! Config `auto_allow` (stage 4) and `Yolo` (stage 5) can hand text to a shell
//! without an interactive review. Both resolve to
//! `crate::tool::RunAuthority::Approved`. Containment depends on the platform.
//!
//! * **macOS / Linux** — [`crate::sandbox`] confines writes to the granted
//!   roots and applies the plan's network policy. The deny floor
//!   being best-effort is survivable here: the sandbox is the boundary. Except
//!   under `[sandbox] mode = "off"`, which removes that boundary on every
//!   platform: this bullet then reads exactly like the Windows one below, and
//!   the deny and irreversible floors are what is left. The mode is a
//!   global-layer-only decision, reported by `doctor`, marked in the status
//!   line, and refused by the eval harness.
//! * **Windows** — a backend exists and reports `available: true`, so commands
//!   are not refused for lack of one, but it is a Job Object:
//!   `sandbox::windows` kills the process tree and caps process count, and
//!   reports `Enforcement::None` for both filesystem and network. Nothing
//!   bounds what such a command writes or sends, so the deny floor
//!   (`shell_deny::builtin_deny`) is the last thing in front of it.
//! * **Every platform** — the per-turn checkpoint snapshots the *primary*
//!   workspace before each turn, so in-workspace damage can be rewound. It is
//!   recovery, not containment: it skips `.git`, `target` and `node_modules`,
//!   does not cover a write root granted later, and neither egress nor a wipe
//!   of the drive holding it respects it.
//!
//! Stated the other way: `yolo` on macOS/Linux is contained by the sandbox;
//! `yolo` on Windows is contained by the deny floor and nothing else, and
//! `SECURITY.md` records the spellings that floor cannot read.
//!
//! Stage 1 supplies hard denials, which stage 3 enforces. Stages 4 and 5 can
//! relax an eligible `NeedsApproval` into running.
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
    ApprovalAuthority, ExecPolicy, NetworkMode, PolicyVerdict, RiskLevel, SandboxMode,
    ToolExecutionPlan, ToolKind, accept_edits_approvable, justification_claimed, network_requested,
    normalize_allow_commands, shell_command_of,
};
pub use permission_mode::{PermissionMode, SharedPermissionMode};
pub use shell_deny::{SafetyNote, safety_notes};
pub use shell_lex::{RunIf, parse_unattended};
