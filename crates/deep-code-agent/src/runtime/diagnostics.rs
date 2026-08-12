use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::lsp::{LspManager, is_edit_tool, render_blocks, summarize_blocks};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::tool::{ToolCall, ToolResult, ToolResultStatus};

impl AgentRuntime {
    /// Enable post-edit LSP diagnostics for the given workspace root.
    #[must_use]
    pub fn with_diagnostics(mut self, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        self.workspace = Some(workspace.clone());
        self.lsp = Some(Arc::new(LspManager::new(workspace)));
        self
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_lsp_manager(mut self, workspace: PathBuf, manager: LspManager) -> Self {
        self.workspace = Some(workspace);
        self.lsp = Some(Arc::new(manager));
        self
    }

    /// Post-edit diagnostics injection: on a successful edit-class tool
    /// result, collect the LSP diagnostics the edit produced, append the
    /// rendered blocks to the model-visible content, and announce them to the
    /// UI via `DiagnosticsUpdated`.
    pub(super) async fn attach_edit_diagnostics(
        &self,
        call: &ToolCall,
        result: &mut ToolResult,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if result.status == ToolResultStatus::Success
            && is_edit_tool(&call.name)
            && let Some(lsp) = self.lsp.as_ref()
        {
            let blocks = lsp.collect_for_edit(&call.name, &call.arguments).await;
            // Operational LSP complaints surface as Warning events — the TUI
            // runs in raw mode, so the manager buffers instead of printing.
            for message in lsp.take_warnings() {
                emit(tx, RuntimeEvent::Warning { message });
            }
            if !blocks.is_empty() {
                let rendered = render_blocks(&blocks);
                let summary = summarize_blocks(&blocks);
                result.content = append_diagnostics(&result.content, &rendered);
                emit(
                    tx,
                    RuntimeEvent::DiagnosticsUpdated {
                        summary: summary.clone(),
                        rendered,
                    },
                );
            }
        }
    }

    /// Flush any buffered LSP warnings as `Warning` events. The manager buffers
    /// (raw-mode terminals can't take a stray `eprintln`); draining here and at
    /// turn end means an operational complaint surfaces even when no edit tool
    /// follows the one that produced it.
    pub(super) async fn drain_lsp_warnings(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        if let Some(lsp) = self.lsp.as_ref() {
            for message in lsp.take_warnings() {
                emit(tx, RuntimeEvent::Warning { message });
            }
        }
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
