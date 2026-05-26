use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool category used by the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnlyFile,
    WriteFile,
    Search,
    Shell,
    GitRead,
    JobControl,
    Mock,
    SubAgent,
    HandleRead,
    Rlm,
    Mcp,
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
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    denied_shell_prefixes: Vec<String>,
    trusted_shell_prefixes: Vec<String>,
    enable_sandbox: bool,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self {
            denied_shell_prefixes: vec![
                "rm -rf".to_string(),
                "sudo ".to_string(),
                "su ".to_string(),
                "curl |".to_string(),
                "wget |".to_string(),
                "chmod 777".to_string(),
                "dd if=".to_string(),
                ":(){ :|:& };:".to_string(),
            ],
            trusted_shell_prefixes: vec![
                "git status".to_string(),
                "git diff".to_string(),
                "git log".to_string(),
                "cargo test".to_string(),
                "cargo build".to_string(),
                "cargo check".to_string(),
                "printf ".to_string(),
                "echo ".to_string(),
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

    #[must_use]
    pub fn with_denied_shell_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.denied_shell_prefixes = prefixes;
        self
    }

    pub fn classify_tool(tool_name: &str) -> ToolKind {
        match tool_name {
            "read_file" | "list_dir" => ToolKind::ReadOnlyFile,
            "grep_files" => ToolKind::Search,
            "write_file" | "apply_patch" => ToolKind::WriteFile,
            "shell_run" | "job_start" => ToolKind::Shell,
            "job_status" | "job_tail" => ToolKind::JobControl,
            "job_cancel" => ToolKind::JobControl,
            name if name.starts_with("git_") => ToolKind::GitRead,
            "mock_echo" => ToolKind::Mock,
            "agent_open" | "agent_eval" | "agent_close" => ToolKind::SubAgent,
            "handle_read" => ToolKind::HandleRead,
            "rlm_open" | "rlm_eval" | "rlm_configure" | "rlm_close" => ToolKind::Rlm,
            name if name.starts_with("mcp__") => ToolKind::Mcp,
            _ => ToolKind::Unknown,
        }
    }

    pub fn evaluate_tool(&self, tool_name: &str, arguments: &Value) -> ToolExecutionPlan {
        let kind = Self::classify_tool(tool_name);
        match kind {
            ToolKind::ReadOnlyFile | ToolKind::Search | ToolKind::GitRead => ToolExecutionPlan {
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
            ToolKind::JobControl => {
                let needs_approval = tool_name == "job_cancel";
                ToolExecutionPlan {
                    verdict: if needs_approval {
                        PolicyVerdict::NeedsApproval {
                            reason: "job_cancel changes process state".to_string(),
                        }
                    } else {
                        PolicyVerdict::Allow
                    },
                    requires_approval: needs_approval,
                    requires_sandbox: false,
                    read_only: tool_name != "job_cancel",
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
                }
            }
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
            ToolKind::Rlm => {
                let needs_approval = matches!(tool_name, "rlm_eval");
                let read_only = matches!(tool_name, "rlm_open" | "rlm_configure" | "rlm_close");
                ToolExecutionPlan {
                    verdict: if needs_approval {
                        PolicyVerdict::NeedsApproval {
                            reason: "rlm_eval executes analysis code against loaded context"
                                .to_string(),
                        }
                    } else {
                        PolicyVerdict::Allow
                    },
                    requires_approval: needs_approval,
                    requires_sandbox: needs_approval && self.enable_sandbox,
                    read_only,
                    risk_level: if needs_approval {
                        RiskLevel::Medium
                    } else {
                        RiskLevel::Low
                    },
                    matched_rule: Some(format!("builtin:{tool_name}")),
                }
            }
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
    let normalized = normalize_command(command);

    for prefix in &policy.denied_shell_prefixes {
        if normalized.starts_with(prefix) {
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

    for prefix in &policy.trusted_shell_prefixes {
        if normalized.starts_with(prefix) {
            return ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: policy.enable_sandbox,
                read_only: false,
                risk_level: RiskLevel::Low,
                matched_rule: Some(format!("trust:{prefix}")),
            };
        }
    }

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

fn normalize_command(command: &str) -> String {
    command.trim().to_ascii_lowercase()
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
    fn handle_read_is_read_only() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("handle_read", &json!({"handle": "h_x", "mode": "head"}));
        assert_eq!(plan.verdict, PolicyVerdict::Allow);
        assert!(plan.read_only);
    }

    #[test]
    fn rlm_eval_requires_approval() {
        let policy = ExecPolicy::default();
        let plan = policy.evaluate_tool("rlm_eval", &json!({"name": "ctx", "code": "stats"}));
        assert!(matches!(plan.verdict, PolicyVerdict::NeedsApproval { .. }));
        assert!(plan.requires_approval);
    }
}
