//! Lazy LSP manager: one transport per language, bounded post-edit polling.

use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone)]
pub struct LspConfig {
    pub enabled: bool,
    /// Wait budget after a routine edit on a warm server.
    pub poll_after_edit_ms: u64,
    /// Longer wait for the first diagnostics after spawning a server.
    pub cold_start_poll_ms: u64,
    pub max_diagnostics_per_file: usize,
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

impl LspConfig {
    fn resolve_command(&self, lang: Language) -> Option<(String, Vec<String>)> {
        let (cmd, args) = server_for(lang)?;
        Some((
            cmd.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
        ))
    }
}

/// Initial spawn plus at most one respawn per language per session — enough
/// to recover from a crashed server without looping on a broken install.
const MAX_SPAWNS_PER_LANGUAGE: u32 = 2;

pub struct LspManager {
    config: LspConfig,
    workspace: PathBuf,
    transports: AsyncMutex<HashMap<Language, Arc<dyn LspTransport>>>,
    missing_warned: AsyncMutex<HashSet<Language>>,
    cold_start_pending: AsyncMutex<HashSet<Language>>,
    spawn_counts: AsyncMutex<HashMap<Language, u32>>,
    test_transports: AsyncMutex<HashMap<Language, Arc<dyn LspTransport>>>,
}

impl LspManager {
    #[must_use]
    pub fn new(config: LspConfig, workspace: PathBuf) -> Self {
        Self {
            config,
            workspace,
            transports: AsyncMutex::new(HashMap::new()),
            missing_warned: AsyncMutex::new(HashSet::new()),
            cold_start_pending: AsyncMutex::new(HashSet::new()),
            spawn_counts: AsyncMutex::new(HashMap::new()),
            test_transports: AsyncMutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self::new(
            LspConfig {
                enabled: false,
                ..LspConfig::default()
            },
            PathBuf::new(),
        )
    }

    #[must_use]
    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    pub async fn install_test_transport(&self, lang: Language, transport: Arc<dyn LspTransport>) {
        self.test_transports.lock().await.insert(lang, transport);
    }

    /// Install into the real transport cache (exercises the drop-on-failure
    /// path, which test transports bypass).
    #[cfg(test)]
    pub(crate) async fn install_transport(&self, lang: Language, transport: Arc<dyn LspTransport>) {
        self.transports.lock().await.insert(lang, transport);
    }

    #[cfg(test)]
    pub(crate) async fn has_transport(&self, lang: Language) -> bool {
        self.transports.lock().await.contains_key(&lang)
    }

    pub async fn collect_for_edit(&self, tool_name: &str, input: &Value) -> Vec<DiagnosticBlock> {
        if !self.config.enabled {
            return Vec::new();
        }
        let relative = edited_paths_for_tool(tool_name, input);
        if relative.is_empty() {
            return Vec::new();
        }
        let paths = resolve_edit_paths(&self.workspace, &relative);
        let mut blocks = Vec::new();
        for path in paths {
            if let Some(block) = self.diagnostics_for(&path).await {
                blocks.push(block);
            }
        }
        blocks
    }

    pub async fn diagnostics_for(&self, file: &Path) -> Option<DiagnosticBlock> {
        if !self.config.enabled {
            return None;
        }
        let lang = detect_language(file);
        if lang == Language::Other {
            return None;
        }

        let text = match tokio::fs::read_to_string(file).await {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "lsp: failed to read {} for diagnostics: {error}",
                    file.display()
                );
                return None;
            }
        };

        let transport = self.transport_for(lang).await?;
        let cold_start = self.cold_start_pending.lock().await.remove(&lang);
        let wait_ms = if cold_start {
            self.config.cold_start_poll_ms
        } else {
            self.config.poll_after_edit_ms
        };
        let wait = Duration::from_millis(wait_ms);
        let raw = match timeout(wait, transport.diagnostics_for(file, &text, wait)).await {
            Ok(Ok(items)) => items,
            Ok(Err(error)) => {
                eprintln!(
                    "lsp: diagnostics call failed for {}: {error}",
                    file.display()
                );
                // A request error usually means the server died. Drop the
                // cached transport so the next edit respawns it lazily
                // (budgeted by MAX_SPAWNS_PER_LANGUAGE). Timeouts above do
                // NOT trigger this — a slow server is not a dead one.
                self.drop_transport(lang).await;
                return None;
            }
            Err(_) => {
                eprintln!("lsp: diagnostics timed out for {}", file.display());
                return None;
            }
        };

        let include_warnings = self.config.include_warnings;
        let mut items: Vec<Diagnostic> = raw
            .into_iter()
            .filter(|item| match item.severity {
                Severity::Error => true,
                Severity::Warning => include_warnings,
                _ => false,
            })
            .collect();
        items.sort_by_key(|item| match item.severity {
            Severity::Error => 0u8,
            Severity::Warning => 1u8,
            Severity::Information => 2u8,
            Severity::Hint => 3u8,
        });

        let mut block = DiagnosticBlock {
            file: relative_to_workspace(&self.workspace, file),
            items,
        };
        block.truncate(self.config.max_diagnostics_per_file);
        if block.items.is_empty() {
            None
        } else {
            Some(block)
        }
    }

    async fn transport_for(&self, lang: Language) -> Option<Arc<dyn LspTransport>> {
        if let Some(transport) = self.test_transports.lock().await.get(&lang) {
            return Some(transport.clone());
        }
        if let Some(transport) = self.transports.lock().await.get(&lang) {
            return Some(transport.clone());
        }

        let (cmd, args) = self.config.resolve_command(lang)?;
        {
            let counts = self.spawn_counts.lock().await;
            if counts.get(&lang).copied().unwrap_or(0) >= MAX_SPAWNS_PER_LANGUAGE {
                return None;
            }
        }
        match StdioLspTransport::spawn(&cmd, &args, lang, self.workspace.clone()).await {
            Ok(transport) => {
                let arc: Arc<dyn LspTransport> = Arc::new(transport);
                self.transports.lock().await.insert(lang, arc.clone());
                self.cold_start_pending.lock().await.insert(lang);
                *self.spawn_counts.lock().await.entry(lang).or_insert(0) += 1;
                Some(arc)
            }
            Err(error) => {
                self.warn_missing_once(lang, &cmd, &error).await;
                None
            }
        }
    }

    /// Remove and shut down a language's cached transport so the next edit
    /// respawns it (within the per-language spawn budget).
    async fn drop_transport(&self, lang: Language) {
        let removed = self.transports.lock().await.remove(&lang);
        if let Some(transport) = removed {
            eprintln!(
                "lsp: dropping the {} server; it will respawn on the next edit",
                lang.as_key()
            );
            transport.shutdown().await;
        }
    }

    async fn warn_missing_once(&self, lang: Language, cmd: &str, error: &anyhow::Error) {
        let mut warned = self.missing_warned.lock().await;
        if warned.insert(lang) {
            eprintln!(
                "lsp: server unavailable for {} (`{cmd}`): {error}; diagnostics disabled for this language",
                lang.as_key()
            );
        }
    }

    pub async fn shutdown_all(&self) {
        let transports: Vec<Arc<dyn LspTransport>> = {
            let mut map = self.transports.lock().await;
            map.drain().map(|(_, transport)| transport).collect()
        };
        for transport in transports {
            transport.shutdown().await;
        }
        self.cold_start_pending.lock().await.clear();
    }
}

fn relative_to_workspace(workspace: &Path, path: &Path) -> PathBuf {
    let workspace = normalize_path(workspace);
    let path = normalize_path(path);
    if let Ok(relative) = path.strip_prefix(&workspace) {
        return relative.to_path_buf();
    }
    PathBuf::from(
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("unknown")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::DiagnosticRange;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTransport {
        items: Vec<Diagnostic>,
        calls: AtomicUsize,
    }

    impl FakeTransport {
        fn new(items: Vec<Diagnostic>) -> Self {
            Self {
                items,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LspTransport for FakeTransport {
        async fn diagnostics_for(
            &self,
            _path: &Path,
            _text: &str,
            _wait: Duration,
        ) -> anyhow::Result<Vec<Diagnostic>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.items.clone())
        }

        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn returns_none_when_disabled() {
        let manager = LspManager::disabled();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.rs");
        tokio::fs::write(&path, b"fn main() {}").await.unwrap();
        assert!(manager.diagnostics_for(&path).await.is_none());
    }

    #[tokio::test]
    async fn forwards_fake_transport_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
        let path = dir.path().join("foo.rs");
        tokio::fs::write(&path, b"let x: i32 = \"oops\";")
            .await
            .unwrap();

        let fake = Arc::new(FakeTransport::new(vec![Diagnostic {
            file: path.clone(),
            range: DiagnosticRange {
                start_line: 1,
                start_column: 14,
                end_line: 1,
                end_column: 15,
            },
            severity: Severity::Error,
            message: "expected i32, found &str".to_string(),
            source: Some("rust-analyzer".to_string()),
            code: None,
        }]));
        manager.install_test_transport(Language::Rust, fake).await;

        let block = manager.diagnostics_for(&path).await.expect("block");
        assert!(block.render().contains("expected i32, found &str"));
    }

    #[tokio::test]
    async fn failed_transport_is_dropped_for_lazy_respawn() {
        struct FailingTransport;

        #[async_trait]
        impl LspTransport for FailingTransport {
            async fn diagnostics_for(
                &self,
                _path: &Path,
                _text: &str,
                _wait: Duration,
            ) -> anyhow::Result<Vec<Diagnostic>> {
                Err(anyhow::anyhow!("server pipe closed"))
            }

            async fn shutdown(&self) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
        let path = dir.path().join("foo.rs");
        tokio::fs::write(&path, b"fn main() {}").await.unwrap();

        manager
            .install_transport(Language::Rust, Arc::new(FailingTransport))
            .await;
        assert!(manager.diagnostics_for(&path).await.is_none());
        assert!(
            !manager.has_transport(Language::Rust).await,
            "dead server must be evicted so the next edit can respawn it"
        );
    }

    #[tokio::test]
    async fn collect_for_edit_resolves_workspace_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
        let path = dir.path().join("src/main.rs");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"fn main() {").await.unwrap();

        let fake = Arc::new(FakeTransport::new(vec![Diagnostic {
            file: path.clone(),
            range: DiagnosticRange {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 2,
            },
            severity: Severity::Error,
            message: "unclosed delimiter".to_string(),
            source: None,
            code: None,
        }]));
        manager.install_test_transport(Language::Rust, fake).await;

        let blocks = manager
            .collect_for_edit(
                "write_file",
                &serde_json::json!({"path": "src/main.rs", "content": "fn main() {"}),
            )
            .await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].file, PathBuf::from("src/main.rs"));
    }
}
