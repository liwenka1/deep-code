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

/// Tunables for the post-edit diagnostics pass — deliberately constants, not
/// config: the on/off switch lives in `[lsp] enabled` (AgentConfig), and
/// nobody realistically tunes poll budgets per project. The defaults trade
/// signal against latency added to every edit.
///
/// Wait budget (ms) for diagnostics from an already-warm server: a warm
/// server usually re-checks a small file in well under a second; five seconds
/// leaves headroom for slower checkers while bounding how long one
/// unresponsive server can stall a turn.
const POLL_AFTER_EDIT_MS: u64 = 5_000;
/// Wait budget (ms) for the first query after spawning a server: the first
/// analysis after a spawn can take tens of seconds on a large workspace
/// (index build).
const COLD_START_POLL_MS: u64 = 30_000;
/// Hard cap on diagnostics kept per file after severity ranking: enough to
/// show real breakage without flooding the model context when one syntax
/// error cascades into dozens of follow-on complaints.
const MAX_DIAGNOSTICS_PER_FILE: usize = 20;
/// Also surface warnings; off so only errors interrupt the turn.
const INCLUDE_WARNINGS: bool = false;

/// Keep only errors (plus warnings when `keep_warnings`), most severe first.
/// Hints never survive. The per-file cap is applied afterwards, on the
/// assembled block.
fn filter_and_rank(mut items: Vec<Diagnostic>, keep_warnings: bool) -> Vec<Diagnostic> {
    items.retain(|item| {
        item.severity == Severity::Error || (keep_warnings && item.severity == Severity::Warning)
    });
    items.sort_by_key(|item| item.severity.rank());
    items
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
    pub fn new(workspace: PathBuf) -> Self {
        Self {
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
            COLD_START_POLL_MS
        } else {
            POLL_AFTER_EDIT_MS
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

        let kept = filter_and_rank(published, INCLUDE_WARNINGS);
        if kept.is_empty() {
            return None;
        }
        let mut block = DiagnosticBlock {
            file: self.display_path(file),
            items: kept,
        };
        block.truncate(MAX_DIAGNOSTICS_PER_FILE);
        Some(block)
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
mod tests;
