//! End-to-end smoke run for the post-edit diagnostics pipeline.
//!
//! Creates a scratch workspace containing one deliberately broken Rust file,
//! then asks the [`LspManager`] to report on it and prints the rendered
//! block. Two modes:
//!
//! * default — spawns a real `rust-analyzer` (must be on PATH),
//! * `--offline` — swaps in a stub transport so no server is needed.
//!
//! ```bash
//! cargo run -p deep-code-agent --example lsp_diagnostics_smoke
//! cargo run -p deep-code-agent --example lsp_diagnostics_smoke -- --offline
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deep_code_agent::{
    Diagnostic, DiagnosticRange, Language, LspConfig, LspManager, LspTransport, Severity,
    render_blocks,
};

const BROKEN_SOURCE: &str = "fn main() { let count: u32 = \"three\"; }";

/// Stand-in for a language server: echoes one canned type error, attributed
/// to whichever file the manager asks about.
struct StubAnalyzer;

#[async_trait]
impl LspTransport for StubAnalyzer {
    async fn diagnostics_for(
        &self,
        path: &Path,
        _text: &str,
        _wait: Duration,
    ) -> anyhow::Result<Vec<Diagnostic>> {
        Ok(vec![Diagnostic {
            file: path.to_path_buf(),
            range: DiagnosticRange {
                start_line: 1,
                start_column: 30,
                end_line: 1,
                end_column: 37,
            },
            severity: Severity::Error,
            message: "mismatched types: expected `u32`, found `&str`".to_owned(),
            source: Some("stub-analyzer".to_owned()),
            code: Some("E0308".to_owned()),
        }])
    }

    async fn shutdown(&self) {}
}

fn have_rust_analyzer() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let offline = std::env::args().any(|arg| arg == "--offline");

    let workspace = tempfile::tempdir()?;
    let broken = workspace.path().join("broken.rs");
    tokio::fs::write(&broken, BROKEN_SOURCE).await?;

    let config = LspConfig::default();
    let cold_budget_ms = config.cold_start_poll_ms;
    let manager = LspManager::new(config, workspace.path().to_path_buf());

    if offline {
        println!("mode: offline (stub transport)");
        manager
            .install_test_transport(Language::Rust, Arc::new(StubAnalyzer))
            .await;
    } else if have_rust_analyzer() {
        println!("mode: live (rust-analyzer, first query may take a while)");
    } else {
        anyhow::bail!("rust-analyzer not found on PATH; install it or pass --offline");
    }

    match manager.diagnostics_for(&broken).await {
        Some(block) => println!("{}", render_blocks(&[block])),
        None if offline => anyhow::bail!("stub transport unexpectedly produced no diagnostics"),
        None => anyhow::bail!(
            "no diagnostics within the {cold_budget_ms}ms cold-start budget \
             (server too slow, spawn failed, or it found nothing wrong)"
        ),
    }

    manager.shutdown_all().await;
    Ok(())
}
