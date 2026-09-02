use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::command_shape;
use super::shell_deny;
use super::shell_lex;

/// Tool category used by the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnlyFile,
    WriteFile,
    Search,
    Shell,
    /// The background-job tool; risk depends on the `action` argument.
    Job,
    Mock,
    SubAgent,
    Network,
    /// `request_write_root`: the model asking to widen the write boundary.
    /// Its whole point is the human decision, so it is never auto-approvable
    /// by any mode, standing consent, or session memory (see
    /// `auto_approval_granted`), and never session-allowable.
    RootGrant,
    Unknown,
}

/// Risk level surfaced to UIs and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
}

impl RiskLevel {
    /// The localization key for this tier's label. Lives on the enum so a UI
    /// renders risk by matching the real variant instead of a `format!("{:?}")`
    /// round-trip — a new variant then fails to compile at the render site
    /// rather than silently falling through to a default colour.
    #[must_use]
    pub fn text_id(self) -> crate::i18n::TextId {
        match self {
            Self::Low => crate::i18n::TextId::RiskLow,
            Self::Medium => crate::i18n::TextId::RiskMedium,
            Self::High => crate::i18n::TextId::RiskHigh,
        }
    }
}

/// Policy outcome before user approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allow,
    Deny { reason: String },
    NeedsApproval { reason: String },
}

/// How sandboxed shell/job commands get network access (`[sandbox] network`).
///
/// Trust ("this command may run") and egress ("it may reach the network") are
/// separate grants: reads stay broad in the sandbox, so pairing them silently
/// turns any auto-allowed command into an exfiltration path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Default: commands run without network unless the call declares
    /// `network: true`, and a declaration always asks the human first
    /// ("approve for session" is remembered per command identity).
    #[default]
    Prompt,
    /// Every sandboxed command gets network without asking (the old coupled
    /// behavior, as an explicit opt-in). Only the user/global layer may set it.
    Always,
    /// Network-declaring commands are refused outright; nothing gets egress.
    Never,
}

impl NetworkMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "prompt" => Some(Self::Prompt),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// The setting spelling, for diagnostics.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// Egress-permissiveness rank: `Never` < `Prompt` < `Always`. The project
    /// config layer may only *lower* this (tighten), never raise it — comparing
    /// against the current value is what stops a repo widening a globally-set
    /// `never` up to `prompt`, not just rejecting the top rung `always`.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::Prompt => 1,
            Self::Always => 2,
        }
    }
}

/// Whether a shell/job call declares it needs network access (`network: true`
/// in the arguments). The declaration comes from the model, but it can only
/// narrow (default is no network) or route into an approval — never grant.
#[must_use]
pub fn network_requested(arguments: &Value) -> bool {
    arguments
        .get("network")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The model's stated reason for a gated call (`justification` in the
/// arguments), for the human at the approval prompt. Advisory text only: it
/// is the model's own claim, never fed back to the auto-mode judge (a
/// classifier reading the requester's sales pitch would let a prompt
/// injection argue itself through) and never a gate input.
#[must_use]
pub fn justification_claimed(arguments: &Value) -> Option<String> {
    arguments
        .get("justification")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Full plan for executing a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPlan {
    pub verdict: PolicyVerdict,
    pub requires_approval: bool,
    pub requires_sandbox: bool,
    pub read_only: bool,
    pub risk_level: RiskLevel,
    pub matched_rule: Option<String>,
    /// Whether the sandbox grants network when this call runs. Only set after
    /// the gate has accounted for it: a declared request under `Prompt` (the
    /// plan then requires approval), or blanket `Always`.
    #[serde(default)]
    pub network: bool,
}

impl ToolExecutionPlan {
    pub fn denied_reason(&self) -> Option<&str> {
        match &self.verdict {
            PolicyVerdict::Deny { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Central execution policy (agent-side, not TUI-specific).
///
/// Shell-command gating is layered and tighten-only: the built-in structured
/// deny rules ([`shell_deny::builtin_deny`]) always run and cannot be removed
/// by configuration. A `trusted_shell_prefix` grants auto-approval, but can
/// never override a deny — deny is evaluated first.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// Auto-approve rules, matched by command identity (`git status` covers
    /// `git status -s` but not `git push`).
    trusted_shell_prefixes: Vec<String>,
    enable_sandbox: bool,
    network_mode: NetworkMode,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self {
            trusted_shell_prefixes: vec![
                "git status".to_string(),
                "git diff".to_string(),
                "git log".to_string(),
                "cargo test".to_string(),
                "cargo build".to_string(),
                "cargo check".to_string(),
                "printf".to_string(),
                "echo".to_string(),
            ],
            enable_sandbox: true,
            network_mode: NetworkMode::Prompt,
        }
    }
}

impl ExecPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_sandbox(mut self, enabled: bool) -> Self {
        self.enable_sandbox = enabled;
        self
    }

    #[must_use]
    pub fn with_network_mode(mut self, mode: NetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    #[must_use]
    pub fn network_mode(&self) -> NetworkMode {
        self.network_mode
    }

    pub fn classify_tool(tool_name: &str) -> ToolKind {
        match tool_name {
            "read_file" | "list_dir" => ToolKind::ReadOnlyFile,
            "grep_files" => ToolKind::Search,
            "write_file" | "apply_patch" => ToolKind::WriteFile,
            "shell" => ToolKind::Shell,
            "job" => ToolKind::Job,
            "web_search" | "fetch_url" => ToolKind::Network,
            "mock_echo" => ToolKind::Mock,
            "agent" => ToolKind::SubAgent,
            "request_write_root" => ToolKind::RootGrant,
            _ => ToolKind::Unknown,
        }
    }

    pub fn evaluate_tool(&self, tool_name: &str, arguments: &Value) -> ToolExecutionPlan {
        let kind = Self::classify_tool(tool_name);
        match kind {
            ToolKind::ReadOnlyFile | ToolKind::Search => ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:read_only_tool".to_string()),
                network: false,
            },
            ToolKind::WriteFile => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "write tools can modify workspace files".to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::Medium,
                matched_rule: Some("builtin:write_tool".to_string()),
                network: false,
            },
            ToolKind::Network => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "network tools can send data to external hosts".to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Medium,
                matched_rule: Some("builtin:network_tool".to_string()),
                network: false,
            },
            ToolKind::Job => match arguments.get("action").and_then(Value::as_str) {
                // Launching a background command is exactly as risky as the
                // command itself: same deny/trust/approve gate as `shell`.
                Some("start") => {
                    let command = arguments
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    evaluate_shell_command(self, command, network_requested(arguments))
                }
                Some("status" | "tail") => ToolExecutionPlan {
                    verdict: PolicyVerdict::Allow,
                    requires_approval: false,
                    requires_sandbox: false,
                    read_only: true,
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
                    network: false,
                },
                Some("cancel") => ToolExecutionPlan {
                    verdict: PolicyVerdict::NeedsApproval {
                        reason: "cancelling a job kills its process".to_string(),
                    },
                    requires_approval: true,
                    requires_sandbox: false,
                    read_only: false,
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
                    network: false,
                },
                // Missing/unknown action: gate defensively; the tool then
                // rejects it as InvalidArguments.
                _ => ToolExecutionPlan {
                    verdict: PolicyVerdict::NeedsApproval {
                        reason: "unknown job action".to_string(),
                    },
                    requires_approval: true,
                    requires_sandbox: false,
                    read_only: false,
                    risk_level: RiskLevel::High,
                    matched_rule: None,
                    network: false,
                },
            },
            ToolKind::Shell => {
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                evaluate_shell_command(self, command, network_requested(arguments))
            }
            ToolKind::Mock => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "mock tool requires approval for tool-loop tests".to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:mock_tool".to_string()),
                network: false,
            },
            // Dispatching a *writing* child is itself the write authorization:
            // `subagent_approval_decision` auto-approves the child's workspace
            // writes on the strength of the dispatch. That authorization must
            // therefore come from the human on the tiers where writes prompt —
            // otherwise spawning an implementer silently downgraded Default's
            // "approve every write" to "approve nothing". Read-only roles keep
            // spawning without a prompt, and `accept_edits_approvable` waves the
            // writing role through on AcceptEdits and above, so the prompt
            // appears exactly where a plain `write_file` would have.
            //
            // A `network: true` dispatch is the network authorization the same
            // way: a child runs unattended, so egress consent cannot be
            // collected at its own prompts (they are auto-denied) — it is
            // collected here, where a human still sees the request. An
            // approved networked child gets the web tools and ambient egress
            // for its allow-listed commands (see `child_tool_registry`); the
            // trusted-command wall itself does not widen.
            ToolKind::SubAgent => {
                let writes = subagent_role_writes(arguments);
                let network = network_requested(arguments);
                // `[sandbox] network = "never"`: a networked dispatch is
                // refused outright, same as a network-declaring shell command
                // — the child would only burn a doomed attempt offline.
                if network && self.network_mode == NetworkMode::Never {
                    return ToolExecutionPlan {
                        verdict: PolicyVerdict::Deny {
                            reason: "network access is disabled by configuration ([sandbox] \
                                     network = \"never\"), so a networked sub-agent cannot run"
                                .to_string(),
                        },
                        requires_approval: false,
                        requires_sandbox: false,
                        read_only: false,
                        risk_level: RiskLevel::Medium,
                        matched_rule: Some("deny:network_disabled".to_string()),
                        network: false,
                    };
                }
                // Under `always`, egress is already ambient for every sandboxed
                // command by explicit config — a networked dispatch adds no
                // consent question. The write authorization still does.
                let network_gated = network && self.network_mode == NetworkMode::Prompt;
                if writes || network_gated {
                    let reason = match (writes, network_gated) {
                        (true, true) => {
                            "dispatching a writing sub-agent with network access authorizes \
                             its workspace writes and its egress — anything it reads may be \
                             sent to external hosts"
                        }
                        (false, true) => {
                            "dispatching a networked sub-agent authorizes its egress — \
                             anything it reads may be sent to external hosts"
                        }
                        _ => "dispatching a writing sub-agent authorizes its workspace writes",
                    };
                    ToolExecutionPlan {
                        verdict: PolicyVerdict::NeedsApproval {
                            reason: reason.to_string(),
                        },
                        requires_approval: true,
                        requires_sandbox: false,
                        read_only: !writes,
                        risk_level: RiskLevel::Medium,
                        matched_rule: Some(
                            if network_gated {
                                "builtin:subagent_network_dispatch"
                            } else {
                                "builtin:subagent_writing_role"
                            }
                            .to_string(),
                        ),
                        network: false,
                    }
                } else {
                    ToolExecutionPlan {
                        verdict: PolicyVerdict::Allow,
                        requires_approval: false,
                        requires_sandbox: false,
                        read_only: true,
                        risk_level: RiskLevel::Low,
                        matched_rule: Some("builtin:subagent_tool".to_string()),
                        network: false,
                    }
                }
            }
            // Widening the write boundary is the highest-consequence request a
            // model can make: everything the sandbox and the path fence deny
            // today becomes allowed under the new root. Always a prompt, top
            // risk tier — and the approval gate hard-excludes it from every
            // auto-approval channel on top of this plan.
            ToolKind::RootGrant => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "grants write access to a directory outside the current roots, \
                             for the rest of the session"
                        .to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::High,
                matched_rule: Some("builtin:root_grant".to_string()),
                network: false,
            },
            ToolKind::Unknown => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: format!("unknown tool '{tool_name}' requires approval"),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::High,
                matched_rule: None,
                network: false,
            },
        }
    }
}

pub fn evaluate_shell_command(
    policy: &ExecPolicy,
    command: &str,
    network_requested: bool,
) -> ToolExecutionPlan {
    // 1. Built-in structured deny (basename + flag aware, segment-split).
    //    Always runs; cannot be disabled by configuration.
    if let Some(reason) = shell_deny::builtin_deny(command) {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Deny {
                reason: format!("shell command denied: {}", reason.0),
            },
            requires_approval: false,
            requires_sandbox: false,
            read_only: false,
            risk_level: RiskLevel::High,
            matched_rule: Some(format!("deny:{}", reason.0)),
            network: false,
        };
    }

    // 2. `[sandbox] network = "never"`: a network-declaring command is refused
    //    outright — running it offline anyway would just burn a doomed attempt.
    if network_requested && policy.network_mode == NetworkMode::Never {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Deny {
                reason: "network access is disabled by configuration ([sandbox] network = \
                         \"never\")"
                    .to_string(),
            },
            requires_approval: false,
            requires_sandbox: false,
            read_only: false,
            risk_level: RiskLevel::Medium,
            matched_rule: Some("deny:network_disabled".to_string()),
            network: false,
        };
    }

    // The grant the sandbox applies once this call actually runs. Under
    // `Prompt` a declaration reaches execution only through the approval
    // forced below (or a standing consent the user granted earlier).
    let network = match policy.network_mode {
        NetworkMode::Always => true,
        NetworkMode::Prompt => network_requested,
        NetworkMode::Never => false,
    };

    let segments = shell_lex::segments(command);
    // Auto-trust only if EVERY segment is covered by a trusted rule
    // (identity-matched, so flags vary but subcommands don't) and the command
    // has no redirection/substitution/expansion — those run programs, write
    // paths, or expand content a trusted prefix doesn't cover.
    let trusted = !segments.is_empty()
        && !shell_lex::has_shell_indirection(command)
        && segments.iter().all(|segment| {
            policy
                .trusted_shell_prefixes
                .iter()
                .any(|prefix| command_shape::rule_covers(prefix, segment))
        });

    // 3. A network declaration under `Prompt` always asks, trusted or not:
    //    egress (or binding a port) is a capability the trust list never
    //    granted. "Approve for session" then remembers the command identity,
    //    so `git push` stops prompting after the first consent.
    if network_requested && policy.network_mode == NetworkMode::Prompt {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::NeedsApproval {
                reason: "the command declares it needs network access (egress or listening)"
                    .to_string(),
            },
            requires_approval: true,
            requires_sandbox: policy.enable_sandbox,
            read_only: false,
            risk_level: if trusted {
                RiskLevel::Medium
            } else {
                RiskLevel::High
            },
            matched_rule: Some("gate:network".to_string()),
            network,
        };
    }

    // 4. Trusted commands run without asking.
    if trusted {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox: policy.enable_sandbox,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: Some("trust:all_segments".to_string()),
            network,
        };
    }

    // 5. Anything else needs explicit user approval.
    ToolExecutionPlan {
        verdict: PolicyVerdict::NeedsApproval {
            reason: "shell commands can modify workspace files or run arbitrary code".to_string(),
        },
        requires_approval: true,
        requires_sandbox: policy.enable_sandbox,
        read_only: false,
        risk_level: RiskLevel::High,
        matched_rule: Some("builtin:shell_default".to_string()),
        network,
    }
}

/// The shell command a tool call would run, if it is command-bearing: the
/// `command` argument for the `shell` tool, or a `job` with `action=start`.
/// `None` for every other tool (and for job status/tail/cancel). One home for
/// the "where does the command live" rule the gate, the safety notes, and
/// session trust all have to agree on (see
/// [`crate::tool::ToolCall::shell_command`], which delegates here).
#[must_use]
pub fn shell_command_of<'a>(tool_name: &str, arguments: &'a Value) -> Option<&'a str> {
    let command_bearing = match ExecPolicy::classify_tool(tool_name) {
        ToolKind::Shell => true,
        ToolKind::Job => arguments.get("action").and_then(Value::as_str) == Some("start"),
        _ => false,
    };
    command_bearing
        .then(|| arguments.get("command").and_then(Value::as_str))
        .flatten()
}

/// Whether a gated call is auto-approvable under `AcceptEdits` mode: a workspace
/// file-edit tool, or an in-workspace filesystem-mutation shell/job command
/// (cc's `acceptEdits` behavior). Everything else still prompts. Hard denials
/// never reach this — they short-circuit in the registry before any decision.
#[must_use]
pub fn accept_edits_approvable(tool_name: &str, arguments: &Value) -> bool {
    // A network declaration is never covered by accept-edits: that mode's
    // standing consent is "edit files in the workspace", not "open egress".
    if network_requested(arguments) {
        return false;
    }
    match ExecPolicy::classify_tool(tool_name) {
        ToolKind::WriteFile => true,
        // Spawning a writing child is standing consent to its workspace writes,
        // which is exactly what AcceptEdits already grants per-write. Only the
        // writing role ever reaches this (read-only spawns don't prompt).
        ToolKind::SubAgent => subagent_role_writes(arguments),
        // Shell / job(start): an in-workspace filesystem-mutation command.
        // `shell_command_of` returns None for job status/tail/cancel, so the
        // bare `Job` arm needs no separate action guard.
        ToolKind::Shell | ToolKind::Job => {
            shell_command_of(tool_name, arguments).is_some_and(shell_deny::is_workspace_fs_edit)
        }
        _ => false,
    }
}

/// Whether an `agent` call's `role` argument names a role whose child may write
/// (see [`crate::subagent::SubAgentRole::allows_writes`]). Absent role means
/// `general` (read-only); an unparsable role fails closed to "writes" — the
/// tool itself will reject it, but if that ever drifts, prompt rather than pass.
fn subagent_role_writes(arguments: &Value) -> bool {
    let role = arguments
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("general");
    crate::subagent::SubAgentRole::parse(role)
        .map(crate::subagent::SubAgentRole::allows_writes)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests;
