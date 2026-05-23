//! Post-edit LSP diagnostics for deep-code-agent.

mod client;
mod diagnostics;
mod hooks;
mod manager;
mod path_util;
mod registry;

pub use client::{LspTransport, StdioLspTransport};
pub use diagnostics::{
    Diagnostic, DiagnosticBlock, DiagnosticRange, Severity, render_blocks, summarize_blocks,
};
#[allow(unused_imports)]
pub use hooks::{edited_paths_for_tool, is_edit_tool, resolve_edit_paths};
pub use manager::{LspConfig, LspManager};
pub use path_util::{normalize_path, paths_equal};
#[allow(unused_imports)]
pub use registry::{Language, detect_language, server_for};
