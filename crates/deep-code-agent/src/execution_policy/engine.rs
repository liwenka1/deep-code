use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::command_shape;
use super::shell_deny;

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
            ToolKind::SubAgent => ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:subagent_tool".to_string()),
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

    let segments = shell_deny::segments(command);
    // Auto-trust only if EVERY segment is covered by a trusted rule
    // (identity-matched, so flags vary but subcommands don't) and the command
    // has no redirection/substitution/expansion — those run programs, write
    // paths, or expand content a trusted prefix doesn't cover.
    let trusted = !segments.is_empty()
        && !shell_deny::has_shell_indirection(command)
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
        ToolKind::Shell => arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(shell_deny::is_workspace_fs_edit),
        ToolKind::Job if arguments.get("action").and_then(Value::as_str) == Some("start") => {
            arguments
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(shell_deny::is_workspace_fs_edit)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accept_edits_approves_file_writes_and_workspace_fs_commands() {
        // File-edit tools always qualify.
        assert!(accept_edits_approvable(
            "write_file",
            &json!({"path": "a.rs"})
        ));
        assert!(accept_edits_approvable(
            "apply_patch",
            &json!({"path": "a.rs"})
        ));
        // In-workspace fs commands (cc's set) qualify.
        assert!(accept_edits_approvable(
            "shell",
            &json!({"command": "mkdir src/new"})
        ));
        assert!(accept_edits_approvable(
            "shell",
            &json!({"command": "mv a.txt b.txt"})
        ));
        // Shell that isn't a bounded fs edit does NOT qualify.
        assert!(!accept_edits_approvable(
            "shell",
            &json!({"command": "cargo build"})
        ));
        assert!(!accept_edits_approvable(
            "shell",
            &json!({"command": "curl https://x"})
        ));
        // Command substitution is never a bounded workspace edit (runs an
        // arbitrary program the allowlist never inspects).
        assert!(!accept_edits_approvable(
            "shell",
            &json!({"command": "touch $(curl http://x/leak)"})
        ));
        // An fs command whose path escapes the workspace now DOES pass this
        // classifier — the OS sandbox denies the out-of-workspace write at
        // execution, so the classifier no longer duplicates that path parsing.
        assert!(accept_edits_approvable(
            "shell",
            &json!({"command": "rm /etc/hosts"})
        ));
        assert!(accept_edits_approvable(
            "shell",
            &json!({"command": "mv ../secret ."})
        ));
        // Network tools never qualify under accept-edits.
        assert!(!accept_edits_approvable(
            "fetch_url",
            &json!({"url": "https://x"})
        ));
        // job start with a workspace fs command qualifies; other actions don't.
        assert!(accept_edits_approvable(
            "job",
            &json!({"action": "start", "command": "touch x"})
        ));
        assert!(!accept_edits_approvable(
            "job",
            &json!({"action": "cancel"})
        ));
    }

    #[test]
    fn read_tools_are_allowed_without_approval() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("read_file", &json!({"path": "a.rs"}));
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(!plan.requires_approval);
        assert!(plan.read_only);
    }

    #[test]
    fn write_tools_need_approval() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("write_file", &json!({"path": "a.rs", "content": "x"}));
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert!(plan.requires_approval);
    }

    #[test]
    fn denied_shell_command_is_blocked() {
        let policy = ExecPolicy::default();
        let plan = evaluate_shell_command(&policy, "rm -rf /", false);
        assert!(matches!(plan.verdict, PolicyVerdict::Deny { .. }));
    }

    #[test]
    fn untrusted_shell_command_needs_approval() {
        let policy = ExecPolicy::default();
        let plan = evaluate_shell_command(&policy, "python exploit.py", false);
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert!(plan.requires_sandbox);
    }

    #[test]
    fn trusted_shell_command_can_run_without_approval() {
        let policy = ExecPolicy::default();
        let plan = evaluate_shell_command(&policy, "cargo test -p deep-code-agent", false);
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(!plan.requires_approval);
    }

    #[test]
    fn absolute_path_destructive_command_cannot_bypass_deny() {
        let policy = ExecPolicy::default();
        // Regression: the old prefix matcher allowed `/bin/rm -rf /` through.
        assert!(matches!(
            evaluate_shell_command(&policy, "/bin/rm -rf /", false).verdict,
            PolicyVerdict::Deny { .. }
        ));
    }

    #[test]
    fn chained_destructive_tail_is_denied_not_trusted() {
        let policy = ExecPolicy::default();
        // A trusted-looking head must not smuggle a destructive tail past the gate.
        assert!(matches!(
            evaluate_shell_command(&policy, "cargo test && rm -rf /", false).verdict,
            PolicyVerdict::Deny { .. }
        ));
    }

    #[test]
    fn trusted_prefix_does_not_extend_to_sibling_subcommand() {
        let policy = ExecPolicy::default();
        // `git status` is trusted; `git push` (not trusted) must ask.
        assert!(matches!(
            evaluate_shell_command(&policy, "git push origin main", false).verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
        // But flags on the trusted prefix stay trusted (identity-matched).
        assert_eq!(
            evaluate_shell_command(&policy, "git status --porcelain", false).verdict,
            PolicyVerdict::Allow
        );
    }

    #[test]
    fn trusted_echo_with_redirection_is_not_auto_allowed() {
        let policy = ExecPolicy::default();
        // `echo` is trusted, but a redirection turns it into a file write.
        assert!(matches!(
            evaluate_shell_command(&policy, "echo pwned > /etc/passwd", false).verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn variable_expansion_is_never_auto_trusted() {
        let policy = ExecPolicy::default();
        // `$VAR` expands to content the reviewer never saw, so a trusted
        // program with an expansion still asks (the indirection gate).
        assert!(matches!(
            evaluate_shell_command(&policy, "echo $HOME", false).verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
        assert!(matches!(
            evaluate_shell_command(&policy, "cargo test ${FLAGS}", false).verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
        // …and never rides the accept-edits allowlist either.
        assert!(!accept_edits_approvable(
            "shell",
            &json!({"command": "mv $SRC dest/"})
        ));
    }

    #[test]
    fn every_segment_must_be_trusted_for_auto_allow() {
        let policy = ExecPolicy::default();
        // `git status` trusted, `python x.py` not → whole command asks.
        assert!(matches!(
            evaluate_shell_command(&policy, "git status && python deploy.py", false).verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
    }

    /// The built-in trust list covers `cargo build/test/check` and `git
    /// status/diff/log`, and matching ignored every flag after the subcommand —
    /// so a redirecting flag rode in on a trusted identity and executed an
    /// arbitrary program with no prompt at *any* permission tier. None of these
    /// contain `$`, `>`, `<` or a backtick, so the structural-indirection gate
    /// never saw them either.
    #[test]
    fn trusted_commands_lose_their_trust_when_a_flag_redirects_execution() {
        let policy = ExecPolicy::default();
        for command in [
            "cargo test --config 'build.rustc-wrapper=\"/tmp/x/wrap\"'",
            "cargo build --config target.x86_64-unknown-linux-gnu.runner=/tmp/r",
            "git diff --output=/tmp/leak",
            "git log --ext-diff",
        ] {
            let plan = evaluate_shell_command(&policy, command, false);
            assert!(
                matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }),
                "{command:?} must reach a human, got {:?}",
                plan.verdict
            );
        }
        // The everyday trusted forms must still run unprompted.
        for command in [
            "cargo build",
            "cargo test --release",
            "cargo test --features full",
            "git diff --stat",
            "git log --oneline -5",
        ] {
            let plan = evaluate_shell_command(&policy, command, false);
            assert_eq!(
                plan.verdict,
                PolicyVerdict::Allow,
                "{command:?} must stay trusted"
            );
        }
    }

    #[test]
    fn network_declaration_forces_approval_even_when_trusted() {
        let policy = ExecPolicy::default();
        // Without the declaration `cargo build` is trusted and runs offline.
        let offline = evaluate_shell_command(&policy, "cargo build", false);
        assert_eq!(offline.verdict, PolicyVerdict::Allow);
        assert!(
            !offline.network,
            "the decoupling: trust no longer grants egress"
        );
        // Declaring network routes the same trusted command into an approval.
        let networked = evaluate_shell_command(&policy, "cargo build", true);
        assert!(matches!(
            networked.verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
        assert!(networked.requires_approval);
        assert_eq!(networked.risk_level, RiskLevel::Medium);
        assert_eq!(networked.matched_rule.as_deref(), Some("gate:network"));
        assert!(networked.network, "an approved run then gets the grant");
        // Untrusted + network keeps the top tier.
        assert_eq!(
            evaluate_shell_command(&policy, "python x.py", true).risk_level,
            RiskLevel::High
        );
    }

    #[test]
    fn network_always_mode_restores_ambient_grant_without_prompting() {
        let policy = ExecPolicy::default().with_network_mode(NetworkMode::Always);
        let plan = evaluate_shell_command(&policy, "cargo build", false);
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(plan.network, "always = every sandboxed run has network");
        // A declaration doesn't force approval either — always is the explicit
        // zero-friction opt-in back to the old coupled behavior.
        let declared = evaluate_shell_command(&policy, "cargo build", true);
        assert_eq!(declared.verdict, PolicyVerdict::Allow);
        assert!(declared.network);
    }

    #[test]
    fn network_never_mode_denies_declared_commands() {
        let policy = ExecPolicy::default().with_network_mode(NetworkMode::Never);
        let plan = evaluate_shell_command(&policy, "git push origin main", true);
        assert!(matches!(plan.verdict, PolicyVerdict::Deny { .. }));
        assert!(!plan.network);
        // Undeclared commands run as usual, just without network.
        let offline = evaluate_shell_command(&policy, "cargo build", false);
        assert_eq!(offline.verdict, PolicyVerdict::Allow);
        assert!(!offline.network);
    }

    #[test]
    fn deny_still_beats_a_network_declaration() {
        let policy = ExecPolicy::default();
        assert!(matches!(
            evaluate_shell_command(&policy, "rm -rf /", true).verdict,
            PolicyVerdict::Deny { .. }
        ));
    }

    #[test]
    fn network_declaration_reaches_shell_and_job_start_via_evaluate_tool() {
        let policy = ExecPolicy::default();
        let shell =
            policy.evaluate_tool("shell", &json!({"command": "cargo build", "network": true}));
        assert_eq!(shell.matched_rule.as_deref(), Some("gate:network"));
        let job = policy.evaluate_tool(
            "job",
            &json!({"action": "start", "command": "cargo build", "network": true}),
        );
        assert_eq!(job.matched_rule.as_deref(), Some("gate:network"));
    }

    #[test]
    fn accept_edits_never_covers_a_network_declaration() {
        // The fs-edit consent is "edit workspace files", not "open egress":
        // the same command that auto-passes offline prompts when it asks for
        // network.
        assert!(accept_edits_approvable(
            "shell",
            &json!({"command": "mkdir src/new"})
        ));
        assert!(!accept_edits_approvable(
            "shell",
            &json!({"command": "mkdir src/new", "network": true})
        ));
        assert!(!accept_edits_approvable(
            "job",
            &json!({"action": "start", "command": "touch x", "network": true})
        ));
    }

    #[test]
    fn job_status_and_tail_are_allowed_read_only() {
        let policy = ExecPolicy::default();
        for action in ["status", "tail"] {
            let plan = policy.evaluate_tool("job", &json!({"action": action, "job_id": "job_1"}));
            assert_eq!(plan.verdict, PolicyVerdict::Allow, "action={action}");
            assert!(plan.read_only, "action={action}");
        }
    }

    #[test]
    fn job_cancel_needs_approval() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("job", &json!({"action": "cancel", "job_id": "job_1"}));
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert!(!plan.read_only);
    }

    #[test]
    fn job_start_inherits_shell_gating() {
        let policy = ExecPolicy::default();
        let denied =
            policy.evaluate_tool("job", &json!({"action": "start", "command": "rm -rf /"}));
        assert!(matches!(denied.verdict, PolicyVerdict::Deny { .. }));

        let trusted =
            policy.evaluate_tool("job", &json!({"action": "start", "command": "cargo test"}));
        assert_eq!(trusted.verdict, PolicyVerdict::Allow);

        let unknown =
            policy.evaluate_tool("job", &json!({"action": "start", "command": "python x.py"}));
        assert!(matches!(
            unknown.verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn unknown_job_action_needs_approval() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("job", &json!({"job_id": "job_1"}));
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert_eq!(plan.risk_level, RiskLevel::High);
    }
}
