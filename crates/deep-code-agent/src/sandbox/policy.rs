use std::path::{Path, PathBuf};

use crate::execution_policy::ToolExecutionPlan;

/// Sandbox restrictions applied to shell commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// No OS sandbox wrapper.
    Unsandboxed,
    /// Read broadly; write only under workspace (and cwd).
    WorkspaceWrite { network_access: bool },
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
            // Network rides the plan, not the trust decision: commands run
            // without egress unless the call declared `network: true` AND the
            // gate accounted for it (a forced approval under the default
            // `prompt` mode, or blanket `[sandbox] network = "always"`). Reads
            // stay broad in the profile, so an ambient network grant would
            // turn every auto-allowed command into an exfiltration path — a
            // no-network run can read `~/.ssh` but cannot send it anywhere.
            // See `NetworkMode` and macos_seatbelt.rs.
            Some(plan) if plan.requires_sandbox => Self::WorkspaceWrite {
                network_access: plan.network,
            },
            _ => Self::Unsandboxed,
        }
    }

    pub fn writable_roots(&self, workspace: &Path, cwd: &Path) -> Vec<PathBuf> {
        match self {
            Self::Unsandboxed => Vec::new(),
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

    fn plan(requires_sandbox: bool, network: bool) -> ToolExecutionPlan {
        ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: None,
            network,
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
        // The decoupling: a sandboxed command WITHOUT a vetted network grant
        // runs with egress (and listening) blocked. Filesystem confinement
        // and the credential-dir write denials are unaffected.
        assert_eq!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, false))),
            SandboxPolicy::WorkspaceWrite {
                network_access: false
            }
        );
        // Only a plan the gate marked (declared + approved, or config
        // `always`) carries network into the sandbox.
        assert_eq!(
            SandboxPolicy::from_execution_plan(Some(&plan(true, true))),
            SandboxPolicy::WorkspaceWrite {
                network_access: true
            }
        );
        assert!(!SandboxPolicy::from_execution_plan(Some(&plan(true, false))).has_network_access());
    }
}
