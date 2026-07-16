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
    HandleRead,
    Mcp,
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

/// Full plan for executing a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPlan {
    pub verdict: PolicyVerdict,
    pub requires_approval: bool,
    pub requires_sandbox: bool,
    pub read_only: bool,
    pub risk_level: RiskLevel,
    pub matched_rule: Option<String>,
}

impl ToolExecutionPlan {
    pub fn allowed(&self) -> bool {
        !matches!(self.verdict, PolicyVerdict::Deny { .. })
    }

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
/// by configuration. Project/user layers may contribute `extra_denied_prefixes`
/// (which add denials) and `trusted_shell_prefixes` (which grant auto-approval),
/// but a trusted prefix can never override a deny — deny is evaluated first.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// Extra deny prefixes contributed by project/user layers, matched at
    /// word boundary against each command segment. The built-in structured
    /// deny always runs regardless of this list.
    extra_denied_prefixes: Vec<String>,
    /// Auto-approve rules, matched by command identity (`git status` covers
    /// `git status -s` but not `git push`).
    trusted_shell_prefixes: Vec<String>,
    enable_sandbox: bool,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self {
            extra_denied_prefixes: Vec::new(),
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

    /// Add extra deny prefixes (project/user layer). These tighten the policy;
    /// the built-in structured deny rules remain in force either way.
    #[must_use]
    pub fn with_denied_shell_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.extra_denied_prefixes = prefixes;
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
            "agent_open" | "agent_eval" | "agent_close" => ToolKind::SubAgent,
            "handle_read" => ToolKind::HandleRead,
            name if name.starts_with("mcp__") => ToolKind::Mcp,
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
            },
            ToolKind::Job => match arguments.get("action").and_then(Value::as_str) {
                // Launching a background command is exactly as risky as the
                // command itself: same deny/trust/approve gate as `shell`.
                Some("start") => {
                    let command = arguments
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    evaluate_shell_command(self, command)
                }
                Some("status" | "tail") => ToolExecutionPlan {
                    verdict: PolicyVerdict::Allow,
                    requires_approval: false,
                    requires_sandbox: false,
                    read_only: true,
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
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
                },
            },
            ToolKind::Shell => {
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                evaluate_shell_command(self, command)
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
            },
            ToolKind::SubAgent => ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:subagent_tool".to_string()),
            },
            ToolKind::HandleRead => ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:handle_read".to_string()),
            },
            ToolKind::Mcp => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: format!("MCP tool '{tool_name}' requires approval"),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::Medium,
                matched_rule: Some("builtin:mcp_tool".to_string()),
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
            },
        }
    }
}

pub fn evaluate_shell_command(policy: &ExecPolicy, command: &str) -> ToolExecutionPlan {
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
        };
    }

    // 2. Extra deny prefixes from project/user layers, matched at word
    //    boundary against every segment (so `foo` denies `foo --bar` but not
    //    `foobar`, and chaining cannot smuggle a denied segment past it).
    let segments = shell_deny::segments(command);
    for prefix in &policy.extra_denied_prefixes {
        let needle = prefix.trim().to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        if segments
            .iter()
            .any(|segment| segment_matches_prefix(segment, &needle))
        {
            return ToolExecutionPlan {
                verdict: PolicyVerdict::Deny {
                    reason: format!("shell command denied by policy rule: {prefix}"),
                },
                requires_approval: false,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::High,
                matched_rule: Some(format!("deny:{prefix}")),
            };
        }
    }

    // 3. Auto-trust only if EVERY segment is covered by a trusted rule
    //    (identity-matched, so flags vary but subcommands don't) and the
    //    command has no redirection or command substitution — those can write
    //    files or run sub-commands a trusted prefix doesn't cover.
    if !segments.is_empty()
        && !has_redirection_or_substitution(command)
        && segments.iter().all(|segment| {
            policy
                .trusted_shell_prefixes
                .iter()
                .any(|prefix| command_shape::rule_covers(prefix, segment))
        })
    {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox: policy.enable_sandbox,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: Some("trust:all_segments".to_string()),
        };
    }

    // 4. Anything else needs explicit user approval.
    ToolExecutionPlan {
        verdict: PolicyVerdict::NeedsApproval {
            reason: "shell commands can modify files, run code, or access the network".to_string(),
        },
        requires_approval: true,
        requires_sandbox: policy.enable_sandbox,
        read_only: false,
        risk_level: RiskLevel::High,
        matched_rule: Some("builtin:shell_default".to_string()),
    }
}

/// Word-boundary prefix match of a deny needle against one command segment.
fn segment_matches_prefix(segment: &str, needle: &str) -> bool {
    let normalized: String = segment
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    normalized == needle
        || (normalized.starts_with(needle)
            && normalized.as_bytes().get(needle.len()) == Some(&b' '))
}

/// True if the command contains shell redirection or command substitution,
/// which disqualifies it from auto-trust (a trusted `echo` must not become an
/// auto-approved file write via `echo x > /etc/passwd`).
fn has_redirection_or_substitution(command: &str) -> bool {
    command.contains('>')
        || command.contains('<')
        || command.contains('`')
        || command.contains("$(")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let plan = evaluate_shell_command(&policy, "rm -rf /");
        assert!(matches!(plan.verdict, PolicyVerdict::Deny { .. }));
    }

    #[test]
    fn untrusted_shell_command_needs_approval() {
        let policy = ExecPolicy::default();
        let plan = evaluate_shell_command(&policy, "python exploit.py");
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert!(plan.requires_sandbox);
    }

    #[test]
    fn trusted_shell_command_can_run_without_approval() {
        let policy = ExecPolicy::default();
        let plan = evaluate_shell_command(&policy, "cargo test -p deep-code-agent");
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(!plan.requires_approval);
    }

    #[test]
    fn absolute_path_destructive_command_cannot_bypass_deny() {
        let policy = ExecPolicy::default();
        // Regression: the old prefix matcher allowed `/bin/rm -rf /` through.
        assert!(matches!(
            evaluate_shell_command(&policy, "/bin/rm -rf /").verdict,
            PolicyVerdict::Deny { .. }
        ));
    }

    #[test]
    fn chained_destructive_tail_is_denied_not_trusted() {
        let policy = ExecPolicy::default();
        // A trusted-looking head must not smuggle a destructive tail past the gate.
        assert!(matches!(
            evaluate_shell_command(&policy, "cargo test && rm -rf /").verdict,
            PolicyVerdict::Deny { .. }
        ));
    }

    #[test]
    fn trusted_prefix_does_not_extend_to_sibling_subcommand() {
        let policy = ExecPolicy::default();
        // `git status` is trusted; `git push` (not trusted) must ask.
        assert!(matches!(
            evaluate_shell_command(&policy, "git push origin main").verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
        // But flags on the trusted prefix stay trusted (identity-matched).
        assert_eq!(
            evaluate_shell_command(&policy, "git status --porcelain").verdict,
            PolicyVerdict::Allow
        );
    }

    #[test]
    fn trusted_echo_with_redirection_is_not_auto_allowed() {
        let policy = ExecPolicy::default();
        // `echo` is trusted, but a redirection turns it into a file write.
        assert!(matches!(
            evaluate_shell_command(&policy, "echo pwned > /etc/passwd").verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn every_segment_must_be_trusted_for_auto_allow() {
        let policy = ExecPolicy::default();
        // `git status` trusted, `python x.py` not → whole command asks.
        assert!(matches!(
            evaluate_shell_command(&policy, "git status && python deploy.py").verdict,
            PolicyVerdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn extra_denied_prefix_from_layer_tightens_policy() {
        let policy =
            ExecPolicy::default().with_denied_shell_prefixes(vec!["kubectl delete".to_string()]);
        // Added deny matches at word boundary, even when chained.
        assert!(matches!(
            evaluate_shell_command(&policy, "kubectl delete pod x").verdict,
            PolicyVerdict::Deny { .. }
        ));
        assert!(matches!(
            evaluate_shell_command(&policy, "echo hi; kubectl delete ns prod").verdict,
            PolicyVerdict::Deny { .. }
        ));
        // Non-matching kubectl subcommand is unaffected by the deny.
        assert!(!matches!(
            evaluate_shell_command(&policy, "kubectl get pods").verdict,
            PolicyVerdict::Deny { .. }
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

    #[test]
    fn handle_read_is_read_only() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("handle_read", &json!({"handle": "h_x", "mode": "head"}));
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(plan.read_only);
    }
}
