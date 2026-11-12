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

#[test]
fn typescript_trust_requires_an_explicit_recognized_value() {
    for value in [
        None,
        Some(""),
        Some("false"),
        Some("0"),
        Some("off"),
        Some("tru"),
        Some("yes"),
    ] {
        assert!(
            !typescript_lsp_trusted(value),
            "{value:?} cannot grant trust"
        );
    }
    for value in ["1", "true", "on", " TRUE ", "On"] {
        assert!(typescript_lsp_trusted(Some(value)), "{value:?}");
    }
}

#[tokio::test]
async fn typescript_trust_gate_runs_before_spawn_and_warns_once_per_language() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new(dir.path().to_path_buf());
    manager.trust_typescript_lsp = false;
    let launches = AtomicU32::new(0);
    for language in [Language::TypeScript, Language::JavaScript] {
        for _ in 0..2 {
            assert!(
                manager
                    .acquire_with(language, |_, _| async {
                        launches.fetch_add(1, Ordering::SeqCst);
                        Ok(Arc::new(CannedTransport(Vec::new())) as Arc<dyn LspTransport>)
                    })
                    .await
                    .is_none()
            );
        }
    }
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    let warnings = manager.take_warnings();
    assert_eq!(warnings.len(), 2, "one warning per language: {warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|warning| warning.contains(TRUST_TYPESCRIPT_LSP_ENV))
    );
    assert!(manager.take_warnings().is_empty());
}

#[tokio::test]
async fn explicit_typescript_trust_preserves_on_demand_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new(dir.path().to_path_buf());
    manager.trust_typescript_lsp = true;
    for language in [Language::TypeScript, Language::JavaScript] {
        let launched = manager
            .acquire_with(language, |program, args| async move {
                assert_eq!(program, "typescript-language-server");
                assert_eq!(args, ["--stdio"]);
                Ok(Arc::new(CannedTransport(Vec::new())) as Arc<dyn LspTransport>)
            })
            .await;
        assert!(launched.is_some());
    }
    assert!(manager.take_warnings().is_empty());
    manager.shutdown_all().await;
}

/// A repository can put its language server first in PATH (e.g. npm run).
/// Default diagnostics must refuse it before executing even its first byte.
#[cfg(unix)]
#[tokio::test]
async fn untrusted_typescript_lsp_never_starts_repository_code() {
    const CHILD: &str = "DEEPCODE_TEST_UNTRUSTED_TYPESCRIPT_LSP";
    if std::env::var_os(CHILD).is_some() {
        let workspace = std::env::current_dir().unwrap();
        let manager = LspManager::new(workspace.clone());
        for file in ["broken.ts", "broken.js"] {
            assert!(
                manager
                    .diagnostics_for(&workspace.join(file))
                    .await
                    .is_none()
            );
        }
        assert!(!workspace.join("server-executed").exists());
        assert_eq!(manager.take_warnings().len(), 2);
        manager.shutdown_all().await;
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let server = bin.join("typescript-language-server");
    std::fs::write(&server, "#!/bin/sh\nprintf executed > server-executed\n").unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        workspace.path().join("broken.ts"),
        "const n: number = 'wrong';",
    )
    .unwrap();
    std::fs::write(workspace.path().join("broken.js"), "const = ;").unwrap();
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lsp::manager::tests::untrusted_typescript_lsp_never_starts_repository_code",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("PATH", &bin)
        .env_remove(TRUST_TYPESCRIPT_LSP_ENV)
        .current_dir(workspace.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("the trust refusal must finish without a server timeout")
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!workspace.path().join("server-executed").exists());
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
    // Give the real server a discoverable crate. A loose file with only a type
    // mismatch can legitimately produce no diagnostics when cargo check is
    // disabled by our security settings. A syntax error in a real crate tests
    // the native diagnostics path without running project build scripts.
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"deep-code-lsp-canary\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    let broken = workspace.path().join("src/main.rs");
    std::fs::write(&broken, "fn main() { let count = ; }\n").unwrap();

    let cold_budget_ms = COLD_START_POLL_MS;
    let manager = LspManager::new(workspace.path().to_path_buf());
    let block = manager.diagnostics_for(&broken).await.unwrap_or_else(|| {
        let warnings = manager.take_warnings();
        panic!(
            "no diagnostics within the {cold_budget_ms}ms cold-start budget \
                 (server too slow, spawn failed, or it found nothing wrong); warnings: {warnings:?}"
        )
    });
    let rendered = crate::lsp::render_blocks(&[block]);
    assert!(
        rendered.contains("main.rs"),
        "diagnostics must reference the broken file, got: {rendered}"
    );
    manager.shutdown_all().await;
}
