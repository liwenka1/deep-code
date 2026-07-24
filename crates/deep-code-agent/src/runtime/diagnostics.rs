use std::path::PathBuf;
use std::sync::Arc;

use crate::lsp::{LspConfig, LspManager};
use crate::runtime::AgentRuntime;

impl AgentRuntime {
    /// Enable post-edit LSP diagnostics for the given workspace root.
    #[must_use]
    pub fn with_diagnostics(mut self, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        self.workspace = Some(workspace.clone());
        self.lsp = Some(Arc::new(LspManager::new(LspConfig::default(), workspace)));
        self
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_lsp_manager(mut self, workspace: PathBuf, manager: LspManager) -> Self {
        self.workspace = Some(workspace);
        self.lsp = Some(Arc::new(manager));
        self
    }
}

pub(super) fn append_diagnostics(content: &str, rendered: &str) -> String {
    if rendered.is_empty() {
        content.to_string()
    } else if content.is_empty() {
        rendered.to_string()
    } else {
        format!("{content}\n\n{rendered}")
    }
}
