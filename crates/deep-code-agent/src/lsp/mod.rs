//! Post-edit LSP diagnostics.
//!
//! After a successful file edit the runtime asks [`LspManager`] for fresh
//! diagnostics on the touched file(s); the rendered result is appended to the
//! tool output so the model sees breakage immediately. Submodules: `registry`
//! (extension → language/server table), `client` (stdio JSON-RPC transport),
//! `manager` (per-language server lifecycle + filtering), `diagnostics`
//! (normalized types + rendering), `hooks` (edit-tool path extraction), and
//! `path_util` (canonical path comparison).

mod client;
mod diagnostics;
mod hooks;
mod manager;
mod path_util;
mod registry;

#[cfg(test)]
pub(crate) use client::LspTransport;
#[cfg(test)]
pub(crate) use diagnostics::{Diagnostic, DiagnosticRange, Severity};
pub use diagnostics::{render_blocks, summarize_blocks};
#[allow(unused_imports)]
pub use hooks::{edited_paths_for_tool, is_edit_tool, resolve_edit_paths};
pub use manager::LspManager;
#[allow(unused_imports)]
pub use path_util::{normalize_path, paths_equal};
#[allow(unused_imports)]
pub use registry::{Language, detect_language, server_for};
