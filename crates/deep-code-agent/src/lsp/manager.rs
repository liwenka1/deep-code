//! Session-scoped coordinator for post-edit diagnostics.
//!
//! The manager owns at most one language server per [`Language`], spawned
//! lazily on the first edit that needs it. Every failure mode degrades to
//! "no diagnostics this time": missing binaries, crashed servers, and slow
//! responses must never block the agent's edit loop.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

use super::client::{LspTransport, StdioLspTransport};
use super::diagnostics::{Diagnostic, DiagnosticBlock, Severity};
use super::hooks::{edited_paths_for_tool, resolve_edit_paths};
use super::path_util::normalize_path;
use super::registry::{Language, detect_language, server_for};

/// Tunables for the post-edit diagnostics pass.
///
/// The defaults trade signal against latency added to every edit:
///
/// * `poll_after_edit_ms = 5_000` — a warm server usually re-checks a small
///   file in well under a second; five seconds leaves headroom for slower
///   checkers while bounding how long one unresponsive server can stall a
///   turn.
/// * `cold_start_poll_ms = 30_000` — the first analysis after a spawn can
///   take tens of seconds on a large workspace (index build), so the first
///   query per server gets a far bigger budget.
/// * `max_diagnostics_per_file = 20` — enough to show real breakage without
///   flooding the model context when a single syntax error cascades into
///   dozens of follow-on complaints.
#[derive(Debug, Clone)]
pub struct LspConfig {
    /// Master switch; `false` turns every entry point into a no-op.
    pub enabled: bool,
    /// Wait budget (ms) for diagnostics from an already-warm server.
    pub poll_after_edit_ms: u64,
    /// Wait budget (ms) for the first query after spawning a server.
    pub cold_start_poll_ms: u64,
    /// Hard cap on diagnostics kept per file after severity ranking.
    pub max_diagnostics_per_file: usize,
    /// Also surface warnings; off by default so only errors interrupt.
    pub include_warnings: bool,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_after_edit_ms: 5_000,
            cold_start_poll_ms: 30_000,
            max_diagnostics_per_file: 20,
            include_warnings: false,
        }
    }
}

/// Spawn ceiling per language per session: the initial launch plus one
/// relaunch after a crash. A server that keeps dying costs at most two
/// spawns instead of one per edit, while a single transient crash still
/// self-heals.
const SPAWN_BUDGET: u32 = 2;

/// Everything tracked per language, guarded together by one lock. The cell
/// lock is held across probe + spawn + install, so concurrent queries for
/// one language wait for a single spawn instead of double-spawning and
/// burning the whole budget at once.
#[derive(Default)]
struct LangCell {
    /// Live connection, if a server was spawned and has not been evicted.
    transport: Option<Arc<dyn LspTransport>>,
    /// Successful spawns so far, compared against [`SPAWN_BUDGET`].
    spawns: u32,
    /// Whether the "server unavailable" notice was already printed.
    notified_unavailable: bool,
}

pub struct LspManager {
    config: LspConfig,
    workspace: PathBuf,
    /// Per-language server state (transport, spawn budget, warning latch).
    /// The outer lock only guards the map itself and is never held across an
    /// await; each cell has its own lock so languages stay independent.
    languages: AsyncMutex<HashMap<Language, Arc<AsyncMutex<LangCell>>>>,
    /// Test seam: a transport registered here bypasses spawning entirely and
    /// always answers with the warm-server wait budget.
    stubs: AsyncMutex<HashMap<Language, Arc<dyn LspTransport>>>,
    /// Operational complaints (unreadable file, dead server, missing binary)
    /// buffered here instead of stderr — the TUI runs in raw mode, so a
    /// direct `eprintln!` would corrupt the screen. The runtime drains this
    /// via [`take_warnings`](Self::take_warnings) and forwards each entry as
    /// a `RuntimeEvent::Warning`.
    warnings: std::sync::Mutex<Vec<String>>,
}

impl LspManager {
    /// Build a manager rooted at `workspace`. No servers are spawned here;
    /// everything is lazy.
    #[must_use]
    pub fn new(config: LspConfig, workspace: PathBuf) -> Self {
        Self {
            config,
            workspace,
            languages: AsyncMutex::new(HashMap::new()),
            stubs: AsyncMutex::new(HashMap::new()),
            warnings: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn warn(&self, message: String) {
        if let Ok(mut warnings) = self.warnings.lock() {
            warnings.push(message);
        }
    }

    /// Drain buffered operational warnings (see the `warnings` field docs).
    #[must_use]
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .map(|mut warnings| std::mem::take(&mut *warnings))
            .unwrap_or_default()
    }

    /// A manager that answers `None`/empty everywhere (`enabled: false` path).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn disabled() -> Self {
        Self::new(
            LspConfig {
                enabled: false,
                ..LspConfig::default()
            },
            PathBuf::new(),
        )
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn config(&self) -> &LspConfig {
        &self.config
    }

    /// Register a stub transport for `lang`, bypassing real server spawns, so
    /// tests can drive the full pipeline without a language server installed.
    #[cfg(test)]
    pub(crate) async fn install_test_transport(
        &self,
        lang: Language,
        transport: Arc<dyn LspTransport>,
    ) {
        self.stubs.lock().await.insert(lang, transport);
    }

    /// Place a transport directly into the real per-language cache, as if it
    /// had been spawned. Exercises eviction paths that stubs bypass.
    #[cfg(test)]
    pub(crate) async fn seed_transport(&self, lang: Language, transport: Arc<dyn LspTransport>) {
        self.cell(lang).await.lock().await.transport = Some(transport);
    }

    #[cfg(test)]
    pub(crate) async fn holds_transport(&self, lang: Language) -> bool {
        let cell = self.languages.lock().await.get(&lang).cloned();
        match cell {
            Some(cell) => cell.lock().await.transport.is_some(),
            None => false,
        }
    }

    /// Entry point for the post-edit hook: extract the file(s) an edit tool
    /// touched from its arguments and gather one block per file that has
    /// something to report.
    pub async fn collect_for_edit(&self, tool_name: &str, input: &Value) -> Vec<DiagnosticBlock> {
        if !self.config.enabled {
            return Vec::new();
        }
        let touched = edited_paths_for_tool(tool_name, input);
        if touched.is_empty() {
            return Vec::new();
        }
        let mut blocks = Vec::new();
        for path in resolve_edit_paths(&self.workspace, &touched) {
            blocks.extend(self.diagnostics_for(&path).await);
        }
        blocks
    }

    /// Ask the responsible server about `file`. Returns `None` whenever there
    /// is nothing useful to say — manager disabled, unsupported language,
    /// unreadable file, no server, timeout, or zero surviving diagnostics.
    pub async fn diagnostics_for(&self, file: &Path) -> Option<DiagnosticBlock> {
        if !self.config.enabled {
            return None;
        }
        let lang = detect_language(file);
        if lang == Language::Other {
            return None;
        }
        let contents = match tokio::fs::read_to_string(file).await {
            Ok(contents) => contents,
            Err(error) => {
                self.warn(format!(
                    "lsp: cannot read {} ({error}); skipping diagnostics",
                    file.display()
                ));
                return None;
            }
        };

        let (transport, first_query) = self.acquire(lang).await?;
        let budget = Duration::from_millis(if first_query {
            self.config.cold_start_poll_ms
        } else {
            self.config.poll_after_edit_ms
        });

        let published =
            match timeout(budget, transport.diagnostics_for(file, &contents, budget)).await {
                Ok(Ok(items)) => items,
                Ok(Err(error)) => {
                    self.warn(format!(
                        "lsp: query for {} failed ({error}); the {} server will respawn on demand",
                        file.display(),
                        lang.as_key()
                    ));
                    // A failed call means the connection itself is gone (dead
                    // process, closed pipes), so evict and let the spawn budget
                    // decide whether a respawn is allowed. A timeout deliberately
                    // does NOT evict: slow is not dead.
                    self.evict(lang).await;
                    return None;
                }
                Err(_) => {
                    self.warn(format!(
                        "lsp: {} produced no diagnostics within {}ms",
                        file.display(),
                        budget.as_millis()
                    ));
                    return None;
                }
            };

        let kept = self.filter_and_rank(published);
        if kept.is_empty() {
            return None;
        }
        let mut block = DiagnosticBlock {
            file: self.display_path(file),
            items: kept,
        };
        block.truncate(self.config.max_diagnostics_per_file);
        Some(block)
    }

    /// Keep only the severities the config asks for, most severe first. The
    /// per-file cap is applied afterwards, on the assembled block.
    fn filter_and_rank(&self, mut items: Vec<Diagnostic>) -> Vec<Diagnostic> {
        let keep_warnings = self.config.include_warnings;
        items.retain(|item| {
            item.severity == Severity::Error
                || (keep_warnings && item.severity == Severity::Warning)
        });
        items.sort_by_key(|item| item.severity.rank());
        items
    }

    /// Fetch (creating on first use) the shared cell for `lang`. The global
    /// lock is only held for the map access, never across an await.
    async fn cell(&self, lang: Language) -> Arc<AsyncMutex<LangCell>> {
        self.languages.lock().await.entry(lang).or_default().clone()
    }

    /// Hand back a usable transport plus whether this is the first query
    /// against a freshly spawned server (which earns the cold-start budget).
    /// Stubs win over real servers; real servers spawn lazily within budget.
    async fn acquire(&self, lang: Language) -> Option<(Arc<dyn LspTransport>, bool)> {
        let workspace = self.workspace.clone();
        self.acquire_with(lang, move |program, args| async move {
            let fresh = StdioLspTransport::spawn(&program, &args, lang, workspace).await?;
            Ok(Arc::new(fresh) as Arc<dyn LspTransport>)
        })
        .await
    }

    /// [`acquire`](Self::acquire) with the server launch injectable, so tests
    /// can observe spawn behavior without a real language server. The cell
    /// lock is held across probe + launch + install: two concurrent queries
    /// for one language get one spawn and one shared transport, while other
    /// languages (own cells) proceed in parallel. A failed launch does not
    /// touch the spawn count — the budget only pays for real spawns.
    async fn acquire_with<F, Fut>(
        &self,
        lang: Language,
        launch: F,
    ) -> Option<(Arc<dyn LspTransport>, bool)>
    where
        F: FnOnce(String, Vec<String>) -> Fut,
        Fut: Future<Output = anyhow::Result<Arc<dyn LspTransport>>>,
    {
        if let Some(stub) = self.stubs.lock().await.get(&lang) {
            return Some((stub.clone(), false));
        }

        let cell = self.cell(lang).await;
        let mut cell = cell.lock().await;
        if let Some(live) = &cell.transport {
            return Some((live.clone(), false));
        }
        if cell.spawns >= SPAWN_BUDGET {
            return None;
        }
        let (program, args) = server_for(lang)?;
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();

        match launch(program.to_owned(), args).await {
            Ok(shared) => {
                cell.spawns += 1;
                cell.transport = Some(shared.clone());
                Some((shared, true))
            }
            Err(error) => {
                if !cell.notified_unavailable {
                    cell.notified_unavailable = true;
                    self.warn(format!(
                        "lsp: `{program}` is not runnable ({error}); {} diagnostics are off",
                        lang.as_key()
                    ));
                }
                None
            }
        }
    }

    /// Drop a language's live transport (shutting it down) so the next edit
    /// can respawn it, still bounded by [`SPAWN_BUDGET`].
    async fn evict(&self, lang: Language) {
        let cell = self.languages.lock().await.get(&lang).cloned();
        let Some(cell) = cell else { return };
        let removed = cell.lock().await.transport.take();
        if let Some(dead) = removed {
            dead.shutdown().await;
        }
    }

    /// Shut down every live server. Called once when the session ends.
    pub async fn shutdown_all(&self) {
        let cells: Vec<Arc<AsyncMutex<LangCell>>> =
            self.languages.lock().await.values().cloned().collect();
        for cell in cells {
            let live = cell.lock().await.transport.take();
            if let Some(transport) = live {
                transport.shutdown().await;
            }
        }
    }

    /// Path shown in rendered blocks: workspace-relative when the file lives
    /// under the workspace, bare file name otherwise (never the raw absolute
    /// path, which would leak machine-specific prefixes into context).
    fn display_path(&self, file: &Path) -> PathBuf {
        let root = normalize_path(&self.workspace);
        let full = normalize_path(file);
        match full.strip_prefix(&root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => full
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("unknown")),
        }
    }
}

#[cfg(test)]
mod tests {
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

    async fn manager_with_stub(
        config: LspConfig,
        items: Vec<Diagnostic>,
    ) -> (tempfile::TempDir, LspManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(config, dir.path().to_path_buf());
        manager
            .install_test_transport(Language::Rust, Arc::new(CannedTransport(items)))
            .await;
        (dir, manager)
    }

    #[tokio::test]
    async fn disabled_manager_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("lib.rs");
        tokio::fs::write(&target, b"fn broken( {").await.unwrap();

        let manager = LspManager::disabled();
        assert!(manager.diagnostics_for(&target).await.is_none());
        assert!(!manager.config().enabled);
    }

    #[tokio::test]
    async fn unsupported_extensions_are_skipped() {
        let (dir, manager) =
            manager_with_stub(LspConfig::default(), vec![diag(1, Severity::Error, "x")]).await;
        let notes = dir.path().join("notes.md");
        tokio::fs::write(&notes, b"# notes").await.unwrap();
        assert!(manager.diagnostics_for(&notes).await.is_none());
    }

    #[tokio::test]
    async fn stub_diagnostics_come_back_with_workspace_relative_paths() {
        let (dir, manager) = manager_with_stub(
            LspConfig::default(),
            vec![diag(7, Severity::Error, "cannot find type `Foo`")],
        )
        .await;
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
        let (dir, silent) = manager_with_stub(LspConfig::default(), items.clone()).await;
        let target = dir.path().join("m.rs");
        tokio::fs::write(&target, b"use x;").await.unwrap();

        let block = silent.diagnostics_for(&target).await.expect("a block");
        assert_eq!(block.items.len(), 1, "only the error survives");
        assert_eq!(block.items[0].severity, Severity::Error);

        let (dir, verbose) = manager_with_stub(
            LspConfig {
                include_warnings: true,
                ..LspConfig::default()
            },
            items,
        )
        .await;
        let target = dir.path().join("m.rs");
        tokio::fs::write(&target, b"use x;").await.unwrap();

        let block = verbose.diagnostics_for(&target).await.expect("a block");
        assert_eq!(block.items.len(), 2, "hints stay excluded either way");
        assert_eq!(
            block.items[0].severity,
            Severity::Error,
            "errors rank ahead of warnings"
        );
        assert_eq!(
            block.items[1].severity,
            Severity::Warning,
            "warning follows"
        );
    }

    #[tokio::test]
    async fn the_per_file_cap_applies_after_ranking() {
        let mut items: Vec<Diagnostic> = (1..=4).map(|n| diag(n, Severity::Warning, "w")).collect();
        items.push(diag(9, Severity::Error, "the real problem"));
        let (dir, manager) = manager_with_stub(
            LspConfig {
                include_warnings: true,
                max_diagnostics_per_file: 2,
                ..LspConfig::default()
            },
            items,
        )
        .await;
        let target = dir.path().join("big.rs");
        tokio::fs::write(&target, b"//").await.unwrap();

        let block = manager.diagnostics_for(&target).await.expect("a block");
        assert_eq!(block.items.len(), 2, "cap of two holds");
        assert_eq!(
            block.items[0].message, "the real problem",
            "ranking must run before the cap so the error is kept"
        );
    }

    #[tokio::test]
    async fn a_failing_transport_is_evicted_for_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
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
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
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
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
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
        let (dir, manager) = manager_with_stub(
            LspConfig::default(),
            vec![diag(1, Severity::Error, "mismatched types")],
        )
        .await;
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

        let config = LspConfig::default();
        let cold_budget_ms = config.cold_start_poll_ms;
        let manager = LspManager::new(config, workspace.path().to_path_buf());
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
}
