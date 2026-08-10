//! Shared helpers for launching a persisted agent runtime (TUI and HTTP).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::client::{DeepSeekClient, LlmClient};
use crate::config::AgentConfig;
use crate::echo_client::EchoClient;
use crate::execution_policy::SharedPermissionMode;
use crate::extensions::{attach_agent_extensions, build_runtime_system_prompt};
use crate::i18n::{Lang, SharedLang};
use crate::runtime::AgentRuntime;
use crate::session_store::{JsonSessionStore, SessionRecord, SessionStore};
use crate::shell_tools::{JobStore, shell_tool_registry};
use crate::subagent::SharedSubAgentManager;
use crate::tool::ToolRegistry;
use crate::workspace_policy::WorkspaceRoots;
use crate::workspace_summary::build_workspace_summary;
use crate::workspace_tools::workspace_tool_registry;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are deep-code's coding assistant.";

/// A launched runtime plus cleanup hooks for sub-agents.
pub struct LaunchedRuntime {
    pub handle: Arc<AgentRuntime>,
    pub backend_label: String,
    pub session_id: Option<String>,
    pub subagent_manager: SharedSubAgentManager,
    pub job_store: JobStore,
    pub stop_hook: Box<dyn Fn() + Send + Sync>,
    /// True when running on the offline placeholder backend (no API key):
    /// the UI should point the user at `/apikey` instead of implying a
    /// working model. Replaces string-sniffing on `backend_label`.
    pub offline: bool,
    /// Non-fatal launch degradations (persistence unavailable, checkpoints
    /// disabled, …). The consumer must surface these — the library never
    /// writes to stderr because a raw-mode TUI may own the screen.
    pub warnings: Vec<String>,
    /// Session permission mode, shared (lock-free) with the runtime's approval
    /// gate. The TUI reads it for the status indicator and flips it on
    /// Shift+Tab; both sides see the same value.
    pub permission_mode: SharedPermissionMode,
    /// Effective extra writable roots this runtime was granted (`--add-dir`,
    /// unioned with a resumed record's own grants). Exposed so consumers can
    /// show the user the real boundary without re-deriving the union.
    pub extra_roots: Vec<PathBuf>,
}

impl LaunchedRuntime {
    pub async fn shutdown(self) {
        (self.stop_hook)();
        // Kill still-running background jobs before tearing the runtime down so
        // dev servers/watchers don't outlive the session. `kill_on_drop` is the
        // backstop for paths that drop the store without calling this.
        self.job_store.shutdown();
        self.handle.shutdown().await;
    }
}

/// Assemble the model-facing tool registry. Workspace and shell tools are L1
/// (unconditional core); web is L2 — a real capability kept built-in but
/// gated at runtime (see [`web_enabled`]).
#[must_use]
pub fn build_tool_registry(
    roots: &WorkspaceRoots,
    network: crate::execution_policy::NetworkMode,
    warnings: &mut Vec<String>,
    ui_lang: &SharedLang,
) -> (ToolRegistry, JobStore) {
    let mut registry = ToolRegistry::new();
    // The exec policy set here is the one the runtime consults for every call,
    // and the one sub-agent registries clone — the single place config-driven
    // gating (network mode) enters.
    registry.set_policy(crate::execution_policy::ExecPolicy::default().with_network_mode(network));
    let mut job_store = JobStore::default();
    match workspace_tool_registry(roots.clone()) {
        Ok(workspace_tools) => registry.extend(workspace_tools),
        Err(error) => warnings.push(format!("workspace tools disabled: {error}")),
    }
    match shell_tool_registry(roots.clone()) {
        Ok((shell_tools, shell_jobs)) => {
            registry.extend(shell_tools);
            job_store = shell_jobs;
        }
        Err(error) => warnings.push(format!("shell tools disabled: {error}")),
    }
    if web_enabled() {
        registry.extend(crate::web_tools::web_tool_registry(ui_lang));
    }
    (registry, job_store)
}

/// Whether the L2 web tools (`web_search`, `fetch_url`) are mounted. On by
/// default; set `DEEP_CODE_DISABLE_WEB` to any non-empty value other than
/// `0`/`false`/`off`/`no` (case-insensitive; blank counts as unset) to gate
/// them off, e.g. for network-restricted or audit-sensitive sessions.
#[must_use]
pub fn web_enabled() -> bool {
    web_enabled_from(
        std::env::var(crate::config::DISABLE_WEB_ENV)
            .ok()
            .as_deref(),
    )
}

fn web_enabled_from(disable_flag: Option<&str>) -> bool {
    match disable_flag {
        None => true,
        // Fail-closed: web stays on only for an explicit "not disabled" value;
        // any other set value (including a typo) disables it, since this gate
        // exists for network-restricted / audit sessions.
        Some(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
    }
}

#[must_use]
pub fn runtime_system_prompt(roots: &WorkspaceRoots) -> String {
    let base = build_runtime_system_prompt(DEFAULT_SYSTEM_PROMPT, &roots.primary);
    let summary = build_workspace_summary(&roots.primary, &roots.extras);
    format!("{base}\n\n{summary}")
}

pub fn launch_runtime(
    config: &AgentConfig,
    roots: impl Into<WorkspaceRoots>,
    resume: Option<SessionRecord>,
) -> LaunchedRuntime {
    let parent_cancel = CancellationToken::new();
    let roots = roots.into();

    if let Some(record) = resume {
        // The record's own workspace stays authoritative on resume (as
        // before); only the extra grants from this command line are merged in.
        return launch_resumed(config, record, roots.extras, &parent_cancel);
    }

    let prompt = runtime_system_prompt(&roots);
    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        return launch_fresh(
            client,
            format!("DeepSeek {}", config.model),
            config,
            roots,
            &prompt,
            &parent_cancel,
            SharedLang::new(Lang::from_env(&config.language)),
        );
    }

    let ui_lang = SharedLang::new(Lang::from_env(&config.language));
    launch_fresh(
        EchoClient::new(ui_lang.clone()),
        "offline echo (set DEEPSEEK_API_KEY for DeepSeek)".to_string(),
        config,
        roots,
        &prompt,
        &parent_cancel,
        ui_lang,
    )
}

/// Launch a new (non-resumed) runtime for any client: persist the session if
/// the store is available, fall back to an in-memory one with a warning.
fn launch_fresh<C: LlmClient + Clone + 'static>(
    client: C,
    backend_label: String,
    config: &AgentConfig,
    roots: WorkspaceRoots,
    prompt: &str,
    parent_cancel: &CancellationToken,
    ui_lang: SharedLang,
) -> LaunchedRuntime {
    let mut warnings = Vec::new();
    let client = Arc::new(client);
    // Decide persistence before assembling tools: the fallback path must not
    // rebuild (and silently drop) a full extensions set, and the failure
    // reason must reach the user instead of being swallowed.
    let persisted = match prepare_persisted_session(&roots, prompt) {
        Ok(pair) => Some(pair),
        Err(reason) => {
            warnings.push(format!(
                "session persistence unavailable; this session will not be saved ({reason})"
            ));
            None
        }
    };
    let (tools, subagent_manager, job_store, shutdown) = build_parent_tools(
        Arc::clone(&client),
        config,
        &roots,
        parent_cancel,
        &mut warnings,
        &ui_lang,
    );
    let (runtime, session_id) = match persisted {
        Some((store, record)) => {
            let session_id = record.id.as_str().to_string();
            (
                AgentRuntime::from_session_record(
                    (*client).clone(),
                    tools,
                    record,
                    store,
                    config.clone(),
                ),
                Some(session_id),
            )
        }
        None => (
            AgentRuntime::with_system_prompt(
                (*client).clone(),
                tools,
                prompt,
                config.clone(),
                false,
            ),
            None,
        ),
    };
    let permission_mode = SharedPermissionMode::new(config.default_permission_mode);
    let runtime = attach_workspace_helpers(runtime, &roots.primary, config, &mut warnings)
        .with_permission_mode(permission_mode.clone())
        .with_ui_lang(ui_lang);
    LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label,
        session_id,
        subagent_manager,
        job_store,
        stop_hook: shutdown,
        offline: client.provider_name() == EchoClient::PROVIDER,
        warnings,
        permission_mode,
        extra_roots: roots.extras,
    }
}

/// Create the session store and save the fresh record, returning the reason
/// on failure so the caller can surface it.
fn prepare_persisted_session(
    roots: &WorkspaceRoots,
    system_prompt: &str,
) -> Result<(JsonSessionStore, SessionRecord), String> {
    let store =
        JsonSessionStore::for_workspace(&roots.primary).map_err(|error| error.to_string())?;
    let record = SessionRecord::new(roots.primary.clone(), system_prompt)
        .with_extra_roots(roots.extras.clone());
    store.save(&record).map_err(|error| error.to_string())?;
    Ok((store, record))
}

fn launch_resumed(
    config: &AgentConfig,
    mut record: SessionRecord,
    cli_extras: Vec<PathBuf>,
    parent_cancel: &CancellationToken,
) -> LaunchedRuntime {
    let mut warnings = Vec::new();
    // A recorded grant whose directory no longer resolves is dropped HERE,
    // with a warning, not handed to WorkspacePolicy. The policy constructor
    // is deliberately fail-closed, and `build_tool_registry` degrades a
    // constructor error into "that tool group is disabled" — so one stale
    // `--add-dir` entry (a sibling repo since deleted) would otherwise strip
    // a resumed session of ALL file and shell tools, with no way to remove
    // the grant. At launch a human is present and the warning is visible;
    // narrowing the session and saying so is the safe direction.
    record.extra_roots.retain(|root| {
        let resolvable = root
            .canonicalize()
            .map(|path| path.is_dir())
            .unwrap_or(false);
        if !resolvable {
            warnings.push(format!(
                "dropping recorded --add-dir grant {}: directory no longer resolves",
                root.display()
            ));
        }
        resolvable
    });
    // Grants persist with the session: the record's extras are restored, and
    // any `--add-dir` passed on the resume command line is merged in. The
    // union is written back through the record, so the next plain `-c` keeps
    // every grant the session ever received.
    for extra in cli_extras {
        if !record.extra_roots.contains(&extra) {
            record.extra_roots.push(extra);
        }
    }
    let roots = WorkspaceRoots::new(record.workspace.clone(), record.extra_roots.clone());
    let workspace = record.workspace.clone();
    let store = match JsonSessionStore::for_workspace(&workspace) {
        Ok(store) => store,
        Err(error) => {
            let mut launched = launch_runtime(config, roots, None);
            launched
                .warnings
                .insert(0, format!("session store unavailable: {error}"));
            launched.warnings.splice(1..1, warnings);
            return launched;
        }
    };

    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        let client = Arc::new(client);
        let ui_lang = SharedLang::new(Lang::from_env(&config.language));
        let (tools, subagent_manager, job_store, shutdown) = build_parent_tools(
            Arc::clone(&client),
            config,
            &roots,
            parent_cancel,
            &mut warnings,
            &ui_lang,
        );
        let permission_mode = SharedPermissionMode::new(config.default_permission_mode);
        let runtime = attach_workspace_helpers(
            AgentRuntime::from_session_record(
                (*client).clone(),
                tools,
                record.clone(),
                store,
                config.clone(),
            ),
            &workspace,
            config,
            &mut warnings,
        )
        .with_permission_mode(permission_mode.clone())
        .with_ui_lang(ui_lang);
        return LaunchedRuntime {
            handle: Arc::new(runtime),
            backend_label: format!("DeepSeek {} (resumed)", config.model),
            session_id: Some(record.id.as_str().to_string()),
            subagent_manager,
            job_store,
            stop_hook: shutdown,
            offline: false,
            warnings,
            permission_mode,
            extra_roots: roots.extras,
        };
    }

    let ui_lang = SharedLang::new(Lang::from_env(&config.language));
    let client = Arc::new(EchoClient::new(ui_lang.clone()));
    let (tools, subagent_manager, job_store, shutdown) = build_parent_tools(
        Arc::clone(&client),
        config,
        &roots,
        parent_cancel,
        &mut warnings,
        &ui_lang,
    );
    let permission_mode = SharedPermissionMode::new(config.default_permission_mode);
    let runtime = attach_workspace_helpers(
        AgentRuntime::from_session_record(
            EchoClient::new(ui_lang.clone()),
            tools,
            record.clone(),
            store,
            config.clone(),
        ),
        &workspace,
        config,
        &mut warnings,
    )
    .with_permission_mode(permission_mode.clone())
    .with_ui_lang(ui_lang);
    LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label: "offline echo (resumed)".to_string(),
        session_id: Some(record.id.as_str().to_string()),
        subagent_manager,
        job_store,
        stop_hook: shutdown,
        offline: true,
        warnings,
        permission_mode,
        extra_roots: roots.extras,
    }
}

fn build_parent_tools<C: LlmClient + 'static>(
    client: Arc<C>,
    config: &AgentConfig,
    roots: &WorkspaceRoots,
    parent_cancel: &CancellationToken,
    warnings: &mut Vec<String>,
    ui_lang: &SharedLang,
) -> (
    ToolRegistry,
    SharedSubAgentManager,
    JobStore,
    Box<dyn Fn() + Send + Sync>,
) {
    let (mut registry, job_store) =
        build_tool_registry(roots, config.sandbox_network, warnings, ui_lang);
    let extensions = attach_agent_extensions(
        &mut registry,
        client,
        config.clone(),
        roots.clone(),
        parent_cancel.clone(),
    );
    let shutdown: Box<dyn Fn() + Send + Sync> = Box::new({
        let extensions = Arc::clone(&extensions);
        move || extensions.cancel_all_running()
    });
    (registry, extensions.subagent_manager(), job_store, shutdown)
}

fn attach_workspace_helpers(
    runtime: AgentRuntime,
    workspace: &Path,
    config: &AgentConfig,
    warnings: &mut Vec<String>,
) -> AgentRuntime {
    let runtime = runtime.with_checkpoints(workspace.to_path_buf(), warnings);
    // `[lsp] enabled = false` skips the manager entirely: no server spawns,
    // no post-edit polling latency.
    if config.lsp_enabled {
        runtime.with_diagnostics(workspace.to_path_buf())
    } else {
        runtime
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::MockEchoTool;

    /// Tool names in the parent registry built for `client`.
    fn parent_tool_names<C: LlmClient + Clone + 'static>(
        client: C,
        config: &AgentConfig,
        workspace: &Path,
    ) -> Vec<String> {
        let cancel = CancellationToken::new();
        let (registry, _, _, _) = build_parent_tools(
            Arc::new(client),
            config,
            &WorkspaceRoots::from(workspace),
            &cancel,
            &mut Vec::new(),
            &SharedLang::default(),
        );
        registry.specs().into_iter().map(|spec| spec.name).collect()
    }

    #[test]
    fn mock_echo_is_mounted_only_for_the_offline_echo_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = AgentConfig {
            api_key: Some("test-key".to_string()),
            ..AgentConfig::builtin()
        };

        let online = DeepSeekClient::new(config.clone()).expect("client builds without network");
        let names = parent_tool_names(online, &config, dir.path());
        assert!(
            !names.iter().any(|name| name == MockEchoTool::NAME),
            "online registry must not expose the mock tool: {names:?}"
        );

        let names = parent_tool_names(
            EchoClient::new(SharedLang::new(Lang::Zh)),
            &config,
            dir.path(),
        );
        assert!(
            !names.iter().any(|name| name == MockEchoTool::NAME),
            "the mock tool is a test fixture; no production registry mounts it: {names:?}"
        );
    }

    #[test]
    fn resume_unions_record_grants_with_cli_add_dirs() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let recorded = tempfile::TempDir::new().unwrap();
        let recorded_root = recorded.path().canonicalize().unwrap();
        let cli = tempfile::TempDir::new().unwrap();
        let cli_root = cli.path().canonicalize().unwrap();

        let record =
            SessionRecord::new(ws.clone(), "system").with_extra_roots(vec![recorded_root.clone()]);
        // No api key → offline echo resume path; the union logic is shared
        // with the online path (it runs before the client branch).
        let config = AgentConfig::builtin();
        let launched = launch_runtime(
            &config,
            WorkspaceRoots::new(ws, vec![cli_root.clone(), recorded_root.clone()]),
            Some(record),
        );
        // Record grants come first, CLI additions after; the repeated
        // `recorded_root` from the CLI must not duplicate.
        assert_eq!(launched.extra_roots, vec![recorded_root, cli_root]);
    }

    #[test]
    fn resume_drops_stale_recorded_grants_instead_of_disabling_tools() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let stale = ws.join("gone");
        std::fs::create_dir(&stale).unwrap();
        let record = SessionRecord::new(ws, "system").with_extra_roots(vec![stale.clone()]);
        std::fs::remove_dir(&stale).unwrap();

        let launched = launch_runtime(
            &AgentConfig::builtin(),
            record.workspace.clone(),
            Some(record),
        );
        // The stale grant is dropped with a warning; it must NOT reach
        // WorkspacePolicy, whose fail-closed constructor would otherwise take
        // every workspace/shell tool down with it on this resumed session —
        // build_tool_registry degrades that error into the "disabled"
        // warnings asserted absent below.
        assert!(launched.extra_roots.is_empty());
        assert!(
            launched
                .warnings
                .iter()
                .any(|warning| warning.contains("gone")),
            "dropped grant must be surfaced: {:?}",
            launched.warnings
        );
        assert!(
            !launched
                .warnings
                .iter()
                .any(|warning| warning.contains("tools disabled")),
            "tools must survive a stale grant: {:?}",
            launched.warnings
        );
    }

    #[test]
    fn web_gate_defaults_on_and_respects_disable_flag() {
        // Unset or explicit "not disabled" values keep web on.
        assert!(web_enabled_from(None));
        assert!(web_enabled_from(Some("0")));
        assert!(web_enabled_from(Some("false")));
        assert!(web_enabled_from(Some("  OFF ")));
        assert!(web_enabled_from(Some("")));
        // Explicit disable values gate it off, case-insensitively.
        assert!(!web_enabled_from(Some("1")));
        assert!(!web_enabled_from(Some("true")));
        assert!(!web_enabled_from(Some("TRUE")));
        assert!(!web_enabled_from(Some("on")));
        assert!(!web_enabled_from(Some("yes")));
        // Fail-closed: an unrecognized/typo value disables rather than leaks web.
        assert!(!web_enabled_from(Some("disabel")));
    }
}
