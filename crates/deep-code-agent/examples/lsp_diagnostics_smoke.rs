//! Smoke example for post-edit LSP diagnostics.
//!
//! Writes a deliberately invalid Rust file and queries diagnostics.
//! Requires `rust-analyzer` on PATH for the live path; use `--offline`
//! to exercise the fake-transport unit path only.
//!
//! ```bash
//! cargo run -p deep-code-agent --example lsp_diagnostics_smoke
//! cargo run -p deep-code-agent --example lsp_diagnostics_smoke -- --offline
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deep_code_agent::{
    Diagnostic, DiagnosticRange, Language, LspConfig, LspManager, LspTransport, Severity,
    render_blocks,
};

struct FakeTransport {
    items: Vec<Diagnostic>,
}

#[async_trait]
impl LspTransport for FakeTransport {
    async fn diagnostics_for(
        &self,
        path: &std::path::Path,
        _text: &str,
        _wait: Duration,
    ) -> anyhow::Result<Vec<Diagnostic>> {
        Ok(self
            .items
            .iter()
            .cloned()
            .map(|mut item| {
                item.file = path.to_path_buf();
                item
            })
            .collect())
    }

    async fn shutdown(&self) {}
}

fn rust_analyzer_available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let offline = std::env::args().any(|arg| arg == "--offline");
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("broken.rs");
    tokio::fs::write(&file, b"fn main() { let x: i32 = \"nope\"; }").await?;

    let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
    if offline {
        manager
            .install_test_transport(
                Language::Rust,
                Arc::new(FakeTransport {
                    items: vec![Diagnostic {
                        file: file.clone(),
                        range: DiagnosticRange {
                            start_line: 1,
                            start_column: 24,
                            end_line: 1,
                            end_column: 30,
                        },
                        severity: Severity::Error,
                        message: "expected i32, found &str".to_string(),
                        source: Some("rust-analyzer".to_string()),
                        code: Some("E0308".to_string()),
                    }],
                }),
            )
            .await;
    } else if !rust_analyzer_available() {
        anyhow::bail!(
            "rust-analyzer is not on PATH; install it or rerun with --offline"
        );
    }

    let block = manager.diagnostics_for(&file).await;
    let Some(block) = block else {
        if offline {
            anyhow::bail!("offline mode expected diagnostics from fake transport");
        }
        anyhow::bail!(
            "no diagnostics returned (possible timeout after {}ms cold-start budget, \
             server spawn failure, or no errors detected)",
            LspConfig::default().cold_start_poll_ms
        );
    };

    let rendered = render_blocks(&[block]);
    println!("{rendered}");

    if !offline {
        println!(
            "live mode: queried invalid file at {}",
            PathBuf::from("broken.rs").display()
        );
    }

    manager.shutdown_all().await;
    Ok(())
}
