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
use crate::shell_tools::{JobStore, shell_tool_registry_from};
use crate::subagent::SharedSubAgentManager;
use crate::tool::ToolRegistry;
use crate::workspace_policy::{WorkspacePolicy, WorkspaceRoots};
use crate::workspace_summary::build_workspace_summary;
use crate::workspace_tools::workspace_tool_registry_from;

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
///
/// Both filesystem-touching groups are built on ONE shared
/// [`WorkspacePolicy`], returned alongside the registry: it is the session's
/// live write boundary, and the runtime widens it in place when the user
/// approves a `request_write_root` — every registered tool and each newly
/// spawned sandboxed command sees the grant immediately, no rebuild. When the
/// boundary cannot be constructed (unresolvable root), both groups are
/// disabled and `request_write_root` is not mounted either — there is no
/// boundary to widen.
#[must_use]
pub fn build_tool_registry(
    roots: &WorkspaceRoots,
    network: crate::execution_policy::NetworkMode,
    warnings: &mut Vec<String>,
    ui_lang: &SharedLang,
) -> (ToolRegistry, JobStore, Option<WorkspacePolicy>) {
    let mut registry = ToolRegistry::new();
    // The exec policy set here is the one the runtime consults for every call,
    // and the one sub-agent registries clone — the single place config-driven
    // gating (network mode) enters.
    registry.set_policy(crate::execution_policy::ExecPolicy::default().with_network_mode(network));
    let mut job_store = JobStore::default();
    let boundary = match WorkspacePolicy::new(roots.clone()) {
        Ok(policy) => Some(policy),
        Err(error) => {
            warnings.push(format!("workspace tools disabled: {error}"));
            warnings.push(format!("shell tools disabled: {error}"));
            None
        }
    };
    if let Some(policy) = &boundary {
        registry.extend(workspace_tool_registry_from(policy.clone()));
        let (shell_tools, shell_jobs) = shell_tool_registry_from(policy.clone());
        registry.extend(shell_tools);
        job_store = shell_jobs;
        registry.register(crate::root_grant::RequestWriteRootTool);
    }
    if web_enabled() {
        registry.extend(crate::web_tools::web_tool_registry(ui_lang));
    }
    (registry, job_store, boundary)
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
    let mut roots = roots.into();
    // `--add-dir $(pwd)` and friends: an extra equal to the primary grants
    // nothing (the workspace is always writable) and would list the workspace
    // as its own "additional" root in the startup banner, summary and record.
    // Enforcement already dedupes inside WorkspacePolicy; this keeps the
    // display surfaces honest. Both spellings are checked because CLI extras
    // arrive canonical while the primary may not be.
    let primary_raw = roots.primary.clone();
    let primary_canonical = crate::paths::canonicalize(&roots.primary).ok();
    roots
        .extras
        .retain(|extra| *extra != primary_raw && Some(extra) != primary_canonical.as_ref());

    if let Some(record) = resume {
        // The workspace the CALLER launched in wins over the one the record
        // claims, and the record's grants are re-vetted. Both because the
        // record is a model-writable file: it lives at
        // `<workspace>/.deep-code/sessions/<id>.json`, inside the primary
        // writable root, and `-c` selects the newest by an `updated_at_ms`
        // read out of that same file. Taking `record.workspace` on trust let a
        // written record move the primary root anywhere — `/` included — and
        // took the session store along with it.
        return launch_resumed(config, record, primary_raw, roots.extras, &parent_cancel);
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
    let ParentTools {
        registry: tools,
        subagent_manager,
        job_store,
        shutdown,
        boundary,
    } = build_parent_tools(
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
        .with_boundary(boundary)
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
    let mut record = SessionRecord::new(roots.primary.clone(), system_prompt)
        .with_extra_roots(roots.extras.clone());
    store.save(&mut record).map_err(|error| error.to_string())?;
    Ok((store, record))
}

fn launch_resumed(
    config: &AgentConfig,
    mut record: SessionRecord,
    caller_workspace: PathBuf,
    cli_extras: Vec<PathBuf>,
    parent_cancel: &CancellationToken,
) -> LaunchedRuntime {
    let mut warnings = Vec::new();
    // Do the grants in this record actually come from this host?
    //
    // The floor below can only refuse what is *categorically* off limits —
    // the filesystem root, the home directory, credential stores. It cannot
    // refuse `~/.cargo` (a `rustc-wrapper` there runs on the next build),
    // `~/Library/LaunchAgents` (runs at next login) or `~/.config/git` (alias
    // code execution), because those are ordinary directories that a human
    // might well grant on purpose. No enumeration of dangerous paths is ever
    // complete, so the grants are authenticated instead of judged: a record
    // the model wrote can still say anything, it just cannot be believed.
    //
    // Checked against the RECORDED workspace, before the substitution below
    // rewrites it — that is what the tag was signed over. A workspace that
    // really moved therefore fails to verify and resumes without its extra
    // grants, which is the safe direction: those roots were vetted relative
    // to somewhere else, and the move is already being reported anyway.
    if !crate::session_integrity::verify_roots(
        record.id.as_str(),
        &record.workspace,
        &record.extra_roots,
        record.extra_roots_mac.as_deref(),
    ) {
        warnings.push(format!(
            "dropping {} write grant(s) recorded in this session: they carry no valid \
             authorship tag for this machine, and the session file is itself writable by \
             the model. Re-grant with --add-dir if you meant them.",
            record.extra_roots.len()
        ));
        record.extra_roots.clear();
    }
    // The primary root is where the USER launched, not where the record says
    // the session lived. The record is an ordinary write target for the model
    // (it sits inside the primary root) and `-c` ranks candidates by a
    // timestamp field read out of the file itself, so `record.workspace` is
    // model-controlled input — taking it on trust moved both the write
    // boundary and the session store wherever it pointed.
    //
    // It also happens to be the right answer for the honest case: a workspace
    // that was moved or copied resumes against where it is now instead of the
    // path it was recorded at.
    // Compared canonically so a benign respelling (`/tmp` vs `/private/tmp` on
    // macOS, a trailing slash) is not reported as a redirect on every resume.
    // Anything that does not resolve to the same directory — including a
    // recorded path that no longer resolves at all — yields to the caller's.
    let same_workspace = match (
        crate::paths::canonicalize(&record.workspace).ok(),
        crate::paths::canonicalize(&caller_workspace).ok(),
    ) {
        (Some(recorded), Some(caller)) => recorded == caller,
        _ => false,
    };
    if !same_workspace {
        warnings.push(format!(
            "resumed session was recorded at {}; using the current workspace {} instead",
            record.workspace.display(),
            caller_workspace.display()
        ));
        record.workspace = caller_workspace;
    }
    // A recorded grant whose directory no longer resolves is dropped HERE,
    // with a warning, not handed to WorkspacePolicy. The policy constructor
    // is deliberately fail-closed, and `build_tool_registry` degrades a
    // constructor error into "that tool group is disabled" — so one stale
    // `--add-dir` entry (a sibling repo since deleted) would otherwise strip
    // a resumed session of ALL file and shell tools, with no way to remove
    // the grant. At launch a human is present and the warning is visible;
    // narrowing the session and saying so is the safe direction.
    let primary_canonical = crate::paths::canonicalize(&record.workspace).ok();
    // `retain_mut`, and the canonical value is written back: the floor below
    // vets `canonical`, but what gets ENFORCED is whatever this vector holds —
    // `WorkspacePolicy::new` canonicalizes it again on its own. Keeping the raw
    // spelling made those two disagree, which is the display-versus-grant split
    // the approval panel was hardened against, reappearing on the persistence
    // channel: a record naming `<workspace>/docs` (a symlink the model planted)
    // showed the user a path inside the repo on the startup banner and in the
    // rebuilt system prompt, while the boundary was wherever the link pointed.
    // A bare `..` was worse still — displayed verbatim, enforced against the
    // process cwd.
    record.extra_roots.retain_mut(|root| {
        let canonical = crate::paths::canonicalize(root)
            .ok()
            .filter(|path| path.is_dir());
        let Some(canonical) = canonical else {
            warnings.push(format!(
                "dropping recorded --add-dir grant {}: directory no longer resolves",
                root.display()
            ));
            return false;
        };
        // Re-vetted against the same floors a model-requested grant clears,
        // for the same reason: a record is a file the model can write, so a
        // root arriving this way has nobody vouching for it either. Without
        // this, writing `extra_roots: ["/"]` (or `~/.ssh`) into a record and
        // letting `-c` pick it up skipped every check the approval flow adds.
        if let Some(reason) = crate::workspace_policy::refuse_as_unattended_root(&canonical) {
            warnings.push(format!(
                "dropping recorded grant {}: {reason}",
                canonical.display()
            ));
            return false;
        }
        // A grant equal to the workspace is covered by the primary root;
        // dropped silently (nothing is lost) so the banner and summary never
        // list the workspace as its own "additional" root.
        if Some(&canonical) == primary_canonical.as_ref() {
            return false;
        }
        *root = canonical;
        true
    });
    // Grants persist with the session: the record's extras are restored, and
    // any `--add-dir` passed on the resume command line is merged in. The
    // union is written back through the record, so the next plain `-c` keeps
    // every grant the session ever received.
    for extra in cli_extras {
        if primary_canonical.as_deref() == Some(extra.as_path()) {
            continue;
        }
        if !record.extra_roots.contains(&extra) {
            record.extra_roots.push(extra);
        }
    }
    let roots = WorkspaceRoots::new(record.workspace.clone(), record.extra_roots.clone());
    // The model reads its write boundary from the system prompt, and the
    // saved prompt names only the grants that existed when the session was
    // created — a root added on `-c --add-dir` (README's own mid-task flow)
    // would be enforceable yet invisible: the summary is what tells the model
    // absolute paths into that root are worth trying at all. Rebuild it from
    // the effective roots; a stale grant dropped above likewise stops being
    // advertised. Costs one provider prefix-cache miss on the resumed
    // conversation; an unusable grant costs the whole feature.
    record.set_system_prompt(runtime_system_prompt(&roots));
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
    // Save the merged grants and refreshed prompt NOW, not at the first
    // turn's persist(): "grants persist with the session" must hold even for
    // a resume that adds a grant and exits before any turn — the next plain
    // `-c` should still see it. A failure degrades to a warning; the
    // persistence actor retries the write on the first turn anyway.
    if let Err(error) = store.save(&mut record) {
        warnings.push(format!("failed to persist resumed session grants: {error}"));
    }

    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        let client = Arc::new(client);
        let ui_lang = SharedLang::new(Lang::from_env(&config.language));
        let ParentTools {
            registry: tools,
            subagent_manager,
            job_store,
            shutdown,
            boundary,
        } = build_parent_tools(
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
        .with_boundary(boundary)
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
    let ParentTools {
        registry: tools,
        subagent_manager,
        job_store,
        shutdown,
        boundary,
    } = build_parent_tools(
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
    .with_boundary(boundary)
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
) -> ParentTools {
    let (mut registry, job_store, boundary) =
        build_tool_registry(roots, config.sandbox_network, warnings, ui_lang);
    // Sub-agents inherit the parent's live boundary; without one (workspace
    // unresolvable — the fs tool groups are disabled too) there is nothing a
    // child could correctly work inside, so the dispatch tool stays unmounted.
    let (subagent_manager, shutdown): (SharedSubAgentManager, Box<dyn Fn() + Send + Sync>) =
        if let Some(policy) = &boundary {
            let extensions = attach_agent_extensions(
                &mut registry,
                client,
                config.clone(),
                policy.clone(),
                parent_cancel.clone(),
            );
            let shutdown: Box<dyn Fn() + Send + Sync> = Box::new({
                let extensions = Arc::clone(&extensions);
                move || extensions.cancel_all_running()
            });
            (extensions.subagent_manager(), shutdown)
        } else {
            warnings.push("sub-agent tools disabled: no workspace boundary".to_string());
            let idle = Arc::new(std::sync::RwLock::new(
                crate::subagent::SubAgentManager::new(0),
            ));
            (idle, Box::new(|| ()))
        };
    ParentTools {
        registry,
        subagent_manager,
        job_store,
        shutdown,
        boundary,
    }
}

/// Everything a launch assembles around the parent registry, in one bundle
/// (five positional returns had become unreadable at the call sites).
struct ParentTools {
    registry: ToolRegistry,
    subagent_manager: SharedSubAgentManager,
    job_store: JobStore,
    shutdown: Box<dyn Fn() + Send + Sync>,
    /// The session's live write boundary; `None` when the workspace could not
    /// be resolved (fs tool groups disabled).
    boundary: Option<WorkspacePolicy>,
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
        let tools = build_parent_tools(
            Arc::new(client),
            config,
            &WorkspaceRoots::from(workspace),
            &cancel,
            &mut Vec::new(),
            &SharedLang::default(),
        );
        tools
            .registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect()
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

    /// The grant doorbell mounts with the fs tool groups — same boundary,
    /// same launch — so any session that can hit the write fence can also
    /// request the fix.
    #[test]
    fn request_write_root_is_mounted_with_the_fs_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = AgentConfig::builtin();
        let names = parent_tool_names(
            EchoClient::new(SharedLang::new(Lang::Zh)),
            &config,
            dir.path(),
        );
        assert!(
            names.iter().any(|name| name == "request_write_root"),
            "grant doorbell missing: {names:?}"
        );
        assert!(names.iter().any(|name| name == "write_file"));
    }

    /// Stamp a record's grants the way a real save does. Tests that build a
    /// record in memory and hand it straight to `launch_runtime` would
    /// otherwise all exercise the unsigned-record rejection instead of the
    /// behaviour they are actually about.
    fn signed(mut record: SessionRecord) -> SessionRecord {
        record.extra_roots_mac = crate::session_integrity::sign_roots(
            record.id.as_str(),
            &record.workspace,
            &record.extra_roots,
        );
        record
    }

    #[test]
    fn resume_unions_record_grants_with_cli_add_dirs() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let recorded = tempfile::TempDir::new().unwrap();
        let recorded_root = recorded.path().canonicalize().unwrap();
        let cli = tempfile::TempDir::new().unwrap();
        let cli_root = cli.path().canonicalize().unwrap();

        let record = signed(
            SessionRecord::new(ws.clone(), "system").with_extra_roots(vec![recorded_root.clone()]),
        );
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
        let record = signed(SessionRecord::new(ws, "system").with_extra_roots(vec![stale.clone()]));
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

    /// A session record does not get to say which workspace it belongs to.
    ///
    /// The record is an ordinary `write_file` target for the model — it lives
    /// at `<workspace>/.deep-code/sessions/<id>.json`, inside the primary
    /// writable root — and `-c` ranks candidates by an `updated_at_ms` read out
    /// of the file itself. Trusting `record.workspace` therefore let a written
    /// record move the primary write root (to `/`, say) and take the session
    /// store with it, without any prompt: not a defeated approval, a skipped
    /// one.
    #[test]
    fn resume_uses_the_callers_workspace_not_the_records_claim() {
        let caller = tempfile::TempDir::new().unwrap();
        let caller_ws = caller.path().canonicalize().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let claimed = elsewhere.path().canonicalize().unwrap();

        let record = SessionRecord::new(claimed.clone(), "system");
        let id = record.id.clone();
        let launched = launch_runtime(&AgentConfig::builtin(), caller_ws.clone(), Some(record));

        assert!(
            launched
                .warnings
                .iter()
                .any(|warning| warning.contains("recorded at")),
            "the redirect must be surfaced, not applied silently: {:?}",
            launched.warnings
        );
        assert!(
            JsonSessionStore::for_workspace(&caller_ws)
                .unwrap()
                .load(&id)
                .is_ok(),
            "the resumed session belongs to the workspace the user launched in"
        );
        assert!(
            JsonSessionStore::for_workspace(&claimed)
                .unwrap()
                .load(&id)
                .is_err(),
            "the record's claim must not redirect the session store"
        );
    }

    /// A recorded grant is re-vetted against the very floors a model-requested
    /// grant has to clear, because a record is a file the model can write.
    ///
    /// Before this, every floor the approval flow added — home, filesystem
    /// root, credential overlap, display-equals-grant — was reachable around by
    /// writing `extra_roots` into a record and letting `-c` pick it up. Those
    /// checks all live in `resolve_grant_target`, which the resume path never
    /// called.
    #[test]
    fn resume_refuses_a_recorded_grant_a_request_could_never_have_won() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
            eprintln!("no resolvable home dir on this host; skipping");
            return;
        };
        if home.starts_with(&ws) || ws.starts_with(&home) {
            eprintln!("tempdir overlaps home on this host; skipping");
            return;
        }

        // Signed, so this test is about the FLOOR: even a grant list this
        // host really did author may not name home or the filesystem root.
        let record = signed(
            SessionRecord::new(ws.clone(), "system")
                .with_extra_roots(vec![home.clone(), PathBuf::from("/")]),
        );
        let launched = launch_runtime(&AgentConfig::builtin(), ws.clone(), Some(record));

        assert!(
            launched.extra_roots.is_empty(),
            "neither the home directory nor the filesystem root may be restored as a \
             write root: {:?}",
            launched.extra_roots
        );
        assert!(
            launched
                .warnings
                .iter()
                .any(|warning| warning.contains("home directory")),
            "the refusal must be surfaced: {:?}",
            launched.warnings
        );
        assert!(
            launched
                .warnings
                .iter()
                .any(|warning| warning.contains("filesystem root")),
            "the refusal must be surfaced: {:?}",
            launched.warnings
        );
    }

    /// What the resume surfaces show and what it enforces must be the same
    /// path. The floor vets the canonical form, but the vector that survives
    /// is what `WorkspacePolicy` re-canonicalizes and what the banner, the
    /// rebuilt system prompt and `LaunchedRuntime::extra_roots` display — so
    /// keeping the record's raw spelling reintroduced, on the persistence
    /// channel, exactly the display-versus-grant split the approval panel was
    /// hardened against. Here the record names a path inside the workspace
    /// that is really a symlink pointing out of it.
    #[cfg(unix)]
    #[test]
    fn resume_displays_the_root_it_actually_enforces() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let real_target = outside.path().canonicalize().unwrap();

        // A spelling that reads as "inside the repo" but resolves out of it.
        let innocuous = ws.join("docs");
        std::os::unix::fs::symlink(&real_target, &innocuous).unwrap();

        let mut record = SessionRecord::new(ws.clone(), "prompt");
        record.extra_roots = vec![innocuous.clone()];
        let record = signed(record);

        let launched = launch_runtime(
            &AgentConfig::builtin(),
            WorkspaceRoots::new(ws.clone(), Vec::new()),
            Some(record),
        );

        assert_eq!(
            launched.extra_roots,
            vec![real_target.clone()],
            "the displayed root must be the one enforced, not the record's spelling"
        );
        assert!(
            !launched.extra_roots.contains(&innocuous),
            "the innocuous-looking spelling must not be what the user is shown"
        );
    }

    /// The record is a file the model can write, and on resume its grants
    /// become the write boundary. The floor can only refuse what is
    /// categorically off limits — it cannot refuse `~/.cargo`, whose
    /// `rustc-wrapper` runs on the next build, or `~/Library/LaunchAgents`,
    /// which runs at next login, because a human might grant either on
    /// purpose. So the grants are authenticated rather than judged: a list
    /// nobody on this host signed is dropped, loudly.
    #[test]
    fn resume_drops_recorded_grants_that_carry_no_authorship_tag() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let smuggled = tempfile::TempDir::new().unwrap();
        let smuggled_root = smuggled.path().canonicalize().unwrap();

        // Exactly what a forged record looks like: a real, resolvable
        // directory that clears every floor, and no tag.
        let mut record = SessionRecord::new(ws.clone(), "system");
        record.extra_roots = vec![smuggled_root.clone()];
        assert!(record.extra_roots_mac.is_none());

        let launched = launch_runtime(&AgentConfig::builtin(), ws.clone(), Some(record));

        assert!(
            launched.extra_roots.is_empty(),
            "an unsigned grant must not become a write root: {:?}",
            launched.extra_roots
        );
        assert!(
            launched
                .warnings
                .iter()
                .any(|warning| warning.contains("authorship tag")),
            "the drop must be surfaced: {:?}",
            launched.warnings
        );

        // A tag for a DIFFERENT grant list must not carry this one either.
        let mut tampered = SessionRecord::new(ws.clone(), "system");
        tampered.extra_roots = vec![ws.join("docs")];
        std::fs::create_dir(ws.join("docs")).unwrap();
        let tampered = signed(tampered);
        let mut tampered = tampered;
        tampered.extra_roots = vec![smuggled_root];
        let launched = launch_runtime(&AgentConfig::builtin(), ws, Some(tampered));
        assert!(
            launched.extra_roots.is_empty(),
            "a tag lifted from another grant list must not verify: {:?}",
            launched.extra_roots
        );
    }

    #[test]
    fn resume_refreshes_system_prompt_and_persists_grants_immediately() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        let extra = tempfile::TempDir::new().unwrap();
        let extra_root = extra.path().canonicalize().unwrap();

        // A session created WITHOUT the grant: its saved prompt cannot name it.
        let store = JsonSessionStore::for_workspace(&ws).unwrap();
        let mut record = SessionRecord::new(ws.clone(), "original prompt");
        let id = record.id.clone();
        store.save(&mut record).unwrap();

        // Resume with `--add-dir extra` (plus a primary-equal extra that the
        // union must skip), then exit without any turn.
        let launched = launch_runtime(
            &AgentConfig::builtin(),
            WorkspaceRoots::new(ws.clone(), vec![extra_root.clone(), ws.clone()]),
            Some(record),
        );
        assert_eq!(launched.extra_roots, vec![extra_root.clone()]);

        // The grant must be on disk already — not parked until the first
        // persist() that a turn would trigger.
        let reloaded = store.load(&id).unwrap();
        assert_eq!(reloaded.extra_roots, vec![extra_root.clone()]);

        // And the stored system prompt now names the root: enforceable but
        // unadvertised is exactly the gap this guards against.
        let crate::session_entry::EntryKind::System { content } = &reloaded.entries[0].kind else {
            panic!("entries[0] must stay the system prompt");
        };
        assert!(
            content.contains("additional writable roots"),
            "refreshed prompt must carry the extras section: {content}"
        );
        assert!(
            content.contains(extra_root.to_string_lossy().as_ref()),
            "refreshed prompt must name the granted root: {content}"
        );
    }

    #[test]
    fn fresh_launch_drops_extras_equal_to_the_primary() {
        let workspace = tempfile::TempDir::new().unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        // `--add-dir $(pwd)`: covered by the primary, so it must not surface
        // as an "additional" root in the banner/summary/record.
        let launched = launch_runtime(
            &AgentConfig::builtin(),
            WorkspaceRoots::new(ws.clone(), vec![ws]),
            None,
        );
        assert!(launched.extra_roots.is_empty());
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
