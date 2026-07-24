use std::path::{Path, PathBuf};

use crate::execution_policy::ToolExecutionPlan;

/// Sandbox restrictions applied to shell commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// No OS sandbox wrapper.
    Unsandboxed,
    /// Read broadly; write only under workspace (and cwd).
    WorkspaceWrite { network_access: bool },
    /// Read-only subprocess.
    ReadOnly,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            network_access: false,
        }
    }
}

impl SandboxPolicy {
    #[must_use]
    pub fn workspace_write() -> Self {
        Self::WorkspaceWrite {
            network_access: false,
        }
    }

    pub fn should_sandbox(&self) -> bool {
        !matches!(self, Self::Unsandboxed)
    }

    pub fn has_network_access(&self) -> bool {
        matches!(
            self,
            Self::WorkspaceWrite {
                network_access: true,
            }
        )
    }

    #[must_use]
    pub fn from_execution_plan(plan: Option<&ToolExecutionPlan>) -> Self {
        match plan {
            Some(plan) if plan.requires_sandbox && plan.read_only => Self::ReadOnly,
            // An approved or trusted shell command runs with network access so
            // `git push` / package installs / `cargo build` work. This is a
            // deliberate trade-off: because reads stay broad, granting egress
            // opens a real exfiltration path (an approved command could read a
            // secret and POST it out). We accept it because (a) every such
            // command is either on the trust list or explicitly approved by the
            // human, and (b) deep-code's OWN key store is read+write sealed in
            // the profile. OS credential dirs (`~/.ssh`, `~/.aws`) stay readable
            // on purpose — sealing them would break the very commands (ssh for
            // `git push`) this grant exists to enable. See macos_seatbelt.rs.
            Some(plan) if plan.requires_sandbox => Self::WorkspaceWrite {
                network_access: true,
            },
            _ => Self::Unsandboxed,
        }
    }

    pub fn writable_roots(&self, workspace: &Path, cwd: &Path) -> Vec<PathBuf> {
        match self {
            Self::Unsandboxed | Self::ReadOnly => Vec::new(),
            Self::WorkspaceWrite { .. } => {
                let mut roots = vec![workspace.to_path_buf()];
                if cwd != workspace {
                    roots.push(cwd.to_path_buf());
                }
                roots
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::{PolicyVerdict, RiskLevel, ToolExecutionPlan};

    fn plan(requires_sandbox: bool, read_only: bool) -> ToolExecutionPlan {
        ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox,
            read_only,
            risk_level: RiskLevel::Low,
            matched_rule: None,
        }
    }

    #[test]
    fn execution_plan_maps_to_sandbox_policy() {
        assert!(matches!(
            SandboxPolicy::from_execution_plan(None),
            SandboxPolicy::Unsandboxed
        ));
        assert!(matches!(
            SandboxPolicy::from_execution_plan(Some(&plan(false, false))),
            SandboxPolicy::Unsandboxed
        ));
        assert!(matches!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, true))),
            SandboxPolicy::ReadOnly
        ));
        // A sandboxed, writable (shell) command is granted network access so
        // git push / installs / cargo build work; filesystem confinement and
        // the credential-dir write denials are unaffected.
        assert_eq!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, false))),
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
        );
        assert!(SandboxPolicy::from_execution_plan(Some(&plan(true, false))).has_network_access());
    }
}
