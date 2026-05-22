use super::{SandboxBackend, SandboxCapabilities};

/// Windows Job Object placeholder — detection only for now.
#[must_use]
pub fn capabilities() -> SandboxCapabilities {
    SandboxCapabilities {
        backend: SandboxBackend::WindowsJobObject,
        available: false,
        detail: "Windows Job Object sandbox is not implemented yet (placeholder)".to_string(),
    }
}
