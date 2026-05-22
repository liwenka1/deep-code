use super::{SandboxBackend, SandboxCapabilities};

/// Linux Landlock placeholder — detection only for now.
#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    SandboxCapabilities {
        backend: SandboxBackend::LinuxLandlock,
        available: false,
        detail: "Landlock sandbox is not implemented yet (placeholder)".to_string(),
    }
}
