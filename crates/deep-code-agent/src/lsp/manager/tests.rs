use super::*;
use crate::lsp::diagnostics::DiagnosticRange;
use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};

fn diag(line: u32, severity: Severity, message: &str) -> Diagnostic {
    Diagnostic {
        file: PathBuf::from("unused-by-manager.rs"),
        range: DiagnosticRange {
            start_line: line,
            start_column: 1,
            end_line: line,
            end_column: 2,
        },
        severity,
        message: message.to_owned(),
        source: None,
        code: None,
    }
}

/// Stub that always answers with a fixed set of diagnostics.
struct CannedTransport(Vec<Diagnostic>);

#[async_trait]
impl LspTransport for CannedTransport {
    async fn diagnostics_for(
        &self,
        _path: &Path,
        _text: &str,
        _wait: Duration,
    ) -> anyhow::Result<Vec<Diagnostic>> {
        Ok(self.0.clone())
    }

    async fn shutdown(&self) {}
}

/// Stub whose every query fails, simulating a dead server connection.
struct BrokenTransport;

#[async_trait]
impl LspTransport for BrokenTransport {
    async fn diagnostics_for(
        &self,
        _path: &Path,
        _text: &str,
        _wait: Duration,
    ) -> anyhow::Result<Vec<Diagnostic>> {
        anyhow::bail!("stdout pipe closed")
    }

    async fn shutdown(&self) {}
}

async fn manager_with_stub(items: Vec<Diagnostic>) -> (tempfile::TempDir, LspManager) {
    let dir = tempfile::tempdir().unwrap();
    let manager = LspManager::new(dir.path().to_path_buf());
    manager
        .install_test_transport(Language::Rust, Arc::new(CannedTransport(items)))
        .await;
    (dir, manager)
}

#[tokio::test]
async fn unsupported_extensions_are_skipped() {
    let (dir, manager) = manager_with_stub(vec![diag(1, Severity::Error, "x")]).await;
    let notes = dir.path().join("notes.md");
    tokio::fs::write(&notes, b"# notes").await.unwrap();
    assert!(manager.diagnostics_for(&notes).await.is_none());
}

#[tokio::test]
async fn stub_diagnostics_come_back_with_workspace_relative_paths() {
    let (dir, manager) =
        manager_with_stub(vec![diag(7, Severity::Error, "cannot find type `Foo`")]).await;
    let target = dir.path().join("src").join("types.rs");
    tokio::fs::create_dir_all(target.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&target, b"type Bar = Foo;").await.unwrap();

    let block = manager.diagnostics_for(&target).await.expect("a block");
    assert_eq!(block.file, PathBuf::from("src/types.rs"));
    assert!(block.render().contains("cannot find type `Foo`"));
}

#[tokio::test]
async fn warnings_are_dropped_unless_opted_in() {
    let items = vec![
        diag(1, Severity::Warning, "unused import"),
        diag(2, Severity::Error, "syntax error"),
        diag(3, Severity::Hint, "consider renaming"),
    ];
    let (dir, silent) = manager_with_stub(items.clone()).await;
    let target = dir.path().join("m.rs");
    tokio::fs::write(&target, b"use x;").await.unwrap();

    let block = silent.diagnostics_for(&target).await.expect("a block");
    assert_eq!(block.items.len(), 1, "only the error survives");
    assert_eq!(block.items[0].severity, Severity::Error);

    // Opted-in filtering is pure logic; drive it directly.
    let kept = filter_and_rank(items, true);
    assert_eq!(kept.len(), 2, "hints stay excluded either way");
    assert_eq!(
        kept[0].severity,
        Severity::Error,
        "errors rank ahead of warnings"
    );
    assert_eq!(kept[1].severity, Severity::Warning, "warning follows");
}

#[test]
fn the_per_file_cap_applies_after_ranking() {
    let mut items: Vec<Diagnostic> = (1..=4).map(|n| diag(n, Severity::Warning, "w")).collect();
    items.push(diag(9, Severity::Error, "the real problem"));
    let mut block = DiagnosticBlock {
        file: PathBuf::from("big.rs"),
        items: filter_and_rank(items, true),
    };
    block.truncate(2);
    assert_eq!(block.items.len(), 2, "cap of two holds");
    assert_eq!(
        block.items[0].message, "the real problem",
        "ranking must run before the cap so the error is kept"
    );
}

#[tokio::test]
async fn a_failing_transport_is_evicted_for_respawn() {
    let dir = tempfile::tempdir().unwrap();
    let manager = LspManager::new(dir.path().to_path_buf());
    let target = dir.path().join("crash.rs");
    tokio::fs::write(&target, b"fn f() {}").await.unwrap();

    manager
        .seed_transport(Language::Rust, Arc::new(BrokenTransport))
        .await;
    assert!(manager.holds_transport(Language::Rust).await);

    assert!(manager.diagnostics_for(&target).await.is_none());
    assert!(
        !manager.holds_transport(Language::Rust).await,
        "a dead connection must leave the cache so the budget can respawn it"
    );
}

#[tokio::test]
async fn concurrent_queries_for_one_language_spawn_a_single_server() {
    let dir = tempfile::tempdir().unwrap();
    let manager = LspManager::new(dir.path().to_path_buf());
    let spawns = AtomicU32::new(0);

    // Slow enough that both acquisitions overlap the launch window.
    let slow_launch = |_program: String, _args: Vec<String>| async {
        spawns.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(Arc::new(CannedTransport(Vec::new())) as Arc<dyn LspTransport>)
    };
    let (first, second) = tokio::join!(
        manager.acquire_with(Language::Rust, slow_launch),
        manager.acquire_with(Language::Rust, slow_launch),
    );

    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "the cell lock must serialize same-language spawns"
    );
    let (first_transport, first_cold) = first.expect("first caller gets a transport");
    let (second_transport, second_cold) = second.expect("second caller gets a transport");
    assert!(
        Arc::ptr_eq(&first_transport, &second_transport),
        "both callers share the one spawned transport"
    );
    assert!(
        first_cold != second_cold,
        "exactly one caller sees the fresh spawn (cold-start budget)"
    );
    assert!(manager.holds_transport(Language::Rust).await);
}

#[tokio::test]
async fn a_failed_launch_does_not_consume_the_spawn_budget() {
    let dir = tempfile::tempdir().unwrap();
    let manager = LspManager::new(dir.path().to_path_buf());
    let attempts = AtomicU32::new(0);

    let failing = |_program: String, _args: Vec<String>| async {
        attempts.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("binary missing")
    };
    // More failed attempts than the budget allows for real spawns…
    for _ in 0..4 {
        assert!(
            manager
                .acquire_with(Language::Rust, failing)
                .await
                .is_none()
        );
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 4);

    // …and a later successful launch still fits inside the budget.
    let working = |_program: String, _args: Vec<String>| async {
        Ok(Arc::new(CannedTransport(Vec::new())) as Arc<dyn LspTransport>)
    };
    assert!(
        manager
            .acquire_with(Language::Rust, working)
            .await
            .is_some(),
        "only real spawns may count against the budget"
    );
}

#[tokio::test]
async fn collect_for_edit_resolves_tool_arguments_to_blocks() {
    let (dir, manager) =
        manager_with_stub(vec![diag(1, Severity::Error, "mismatched types")]).await;
    let target = dir.path().join("edited.rs");
    tokio::fs::write(&target, b"let x: u8 = -1;").await.unwrap();

    let blocks = manager
        .collect_for_edit(
            "write_file",
            &serde_json::json!({ "path": "edited.rs", "content": "let x: u8 = -1;" }),
        )
        .await;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].file, PathBuf::from("edited.rs"));

    let none = manager
        .collect_for_edit("read_file", &serde_json::json!({ "path": "edited.rs" }))
        .await;
    assert!(none.is_empty(), "non-edit tools trigger no diagnostics");
}

/// Real language-server smoke: spawns an actual rust-analyzer against a
/// deliberately broken file. Run manually (cold start can take a while):
/// `cargo test -p deep-code-agent lsp -- --ignored`
#[tokio::test]
#[ignore = "requires rust-analyzer on PATH"]
async fn real_rust_analyzer_reports_diagnostics_for_broken_file() {
    let available = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(available, "rust-analyzer not found on PATH");

    let workspace = tempfile::tempdir().unwrap();
    let broken = workspace.path().join("broken.rs");
    std::fs::write(&broken, "fn main() { let count: u32 = \"three\"; }").unwrap();

    let cold_budget_ms = COLD_START_POLL_MS;
    let manager = LspManager::new(workspace.path().to_path_buf());
    let block = manager.diagnostics_for(&broken).await.unwrap_or_else(|| {
        panic!(
            "no diagnostics within the {cold_budget_ms}ms cold-start budget \
                 (server too slow, spawn failed, or it found nothing wrong)"
        )
    });
    let rendered = crate::lsp::render_blocks(&[block]);
    assert!(
        rendered.contains("broken.rs"),
        "diagnostics must reference the broken file, got: {rendered}"
    );
    manager.shutdown_all().await;
}
