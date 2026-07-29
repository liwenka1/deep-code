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
pub use hooks::is_edit_tool;
pub use manager::LspManager;
#[cfg(test)]
pub(crate) use registry::Language;
