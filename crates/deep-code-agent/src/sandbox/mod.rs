//! OS sandbox helpers for shell execution.

mod policy;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos_seatbelt;
#[cfg(target_os = "windows")]
mod windows;

pub use policy::SandboxPolicy;

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Detected sandbox backend for the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxBackend {
    #[default]
    None,
    #[cfg(target_os = "macos")]
    MacosSeatbelt,
    #[cfg(target_os = "linux")]
    LinuxLandlock,
    #[cfg(target_os = "windows")]
    WindowsJobObject,
}

impl SandboxBackend {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            #[cfg(target_os = "macos")]
            Self::MacosSeatbelt => "macos_seatbelt",
            #[cfg(target_os = "linux")]
            Self::LinuxLandlock => "linux_landlock",
            #[cfg(target_os = "windows")]
            Self::WindowsJobObject => "windows_job_object",
        }
    }
}

/// Platform sandbox capability report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub backend: SandboxBackend,
    pub available: bool,
    pub detail: String,
}

/// Probe sandbox support on this host.
#[must_use]
pub fn detect_capabilities() -> SandboxCapabilities {
    #[cfg(target_os = "macos")]
    {
        if macos_seatbelt::is_available() {
            return SandboxCapabilities {
                backend: SandboxBackend::MacosSeatbelt,
                available: true,
                detail: "sandbox-exec (Seatbelt) is available".to_string(),
            };
        }
        SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            detail: "sandbox-exec is missing or not permitted".to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    {
        return linux::capabilities();
    }

    #[cfg(target_os = "windows")]
    {
        return windows::capabilities();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        SandboxCapabilities {
            backend: SandboxBackend::None,
            available: false,
            detail: "unsupported platform".to_string(),
        }
    }
}

/// Prepares subprocess commands, optionally wrapping them in an OS sandbox.
#[derive(Debug, Clone, Default)]
pub struct SandboxManager {
    forced: Option<bool>,
}

impl SandboxManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force sandbox on/off for tests.
    #[must_use]
    pub fn force_sandbox(mut self, enabled: Option<bool>) -> Self {
        self.forced = enabled;
        self
    }

    pub fn should_sandbox(&self, policy: &SandboxPolicy) -> bool {
        if !policy.should_sandbox() {
            return false;
        }
        if let Some(forced) = self.forced {
            return forced;
        }
        detect_capabilities().available
    }

    pub fn wrap_shell_command(
        &self,
        command: &str,
        cwd: &Path,
        workspace: &Path,
        policy: &SandboxPolicy,
    ) -> Command {
        if !self.should_sandbox(policy) {
            return bare_shell_command(command, cwd);
        }

        #[cfg(target_os = "macos")]
        {
            macos_seatbelt::wrap_shell_command(command, cwd, workspace, policy)
        }

        #[cfg(target_os = "linux")]
        {
            linux::wrap_shell_command(command, cwd, workspace, policy)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = workspace;
            bare_shell_command(command, cwd)
        }
    }
}

fn bare_shell_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    cmd.current_dir(cwd);
    cmd
}

static CAPABILITIES: OnceLock<SandboxCapabilities> = OnceLock::new();

/// Cached capability probe.
#[must_use]
pub fn capabilities() -> &'static SandboxCapabilities {
    CAPABILITIES.get_or_init(detect_capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_capabilities_returns_structured_report() {
        let caps = detect_capabilities();
        assert!(!caps.detail.is_empty());
    }
}
