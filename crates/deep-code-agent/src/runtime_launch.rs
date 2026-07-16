//! Shared helpers for launching a persisted agent runtime (TUI and HTTP).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::client::{DeepSeekClient, LlmClient};
use crate::config::AgentConfig;
use crate::echo_client::EchoClient;
use crate::extensions::{RuntimeBootstrap, attach_agent_extensions, build_runtime_system_prompt};
use crate::plan_mode::PlanMode;
use crate::runtime::{AgentRuntime, AgentRuntimeHandle};
use crate::session_store::{
    ConfigSnapshot, JsonSessionStore, SessionId, SessionRecord, SessionStore,
};
use crate::shell_tools::{JobStore, shell_tool_registry};
use crate::subagent::SharedSubAgentManager;
use crate::tool::ToolRegistry;
use crate::workspace_summary::build_workspace_summary;
use crate::workspace_tools::workspace_tool_registry;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are deep-code's coding assistant.";

/// A launched runtime plus cleanup hooks for sub-agents and MCP.
pub struct LaunchedRuntime {
    pub handle: Arc<dyn AgentRuntimeHandle>,
    pub backend_label: String,
    pub session_id: Option<String>,
    pub subagent_manager: SharedSubAgentManager,
    pub job_store: JobStore,
    pub plan_mode: PlanMode,
    pub stop_hook: Box<dyn Fn() + Send + Sync>,
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
/// (unconditional core); web is L2 (a real capability, kept built-in, with a
/// runtime gate added later).
#[must_use]
pub fn build_tool_registry(workspace: &Path) -> (ToolRegistry, JobStore) {
    let mut registry = ToolRegistry::with_mock_tools();
    let mut job_store = JobStore::default();
    match workspace_tool_registry(workspace.to_path_buf()) {
        Ok(workspace_tools) => registry.extend(workspace_tools),
        Err(error) => eprintln!("workspace tools disabled: {error}"),
    }
    match shell_tool_registry(workspace.to_path_buf()) {
        Ok((shell_tools, shell_jobs)) => {
            registry.extend(shell_tools);
            job_store = shell_jobs;
        }
        Err(error) => eprintln!("shell tools disabled: {error}"),
    }
    registry.extend(crate::web_tools::web_tool_registry());
    (registry, job_store)
}

#[must_use]
pub fn runtime_system_prompt(workspace: &Path) -> String {
    let base = build_runtime_system_prompt(DEFAULT_SYSTEM_PROMPT, workspace);
    let summary = build_workspace_summary(workspace);
    format!("{base}\n\n{summary}")
}

pub fn launch_runtime(
    config: &AgentConfig,
    workspace: PathBuf,
    resume: Option<SessionRecord>,
) -> LaunchedRuntime {
    let parent_cancel = CancellationToken::new();
    let prompt = runtime_system_prompt(&workspace);

    if let Some(record) = resume {
        return launch_resumed(config, record, &parent_cancel);
    }

    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        return launch_fresh(
            client,
            format!("DeepSeek {}", config.model),
            config,
            workspace,
            &prompt,
            &parent_cancel,
        );
    }

    launch_fresh(
        EchoClient,
        "offline echo (set DEEPSEEK_API_KEY for DeepSeek)".to_string(),
        config,
        workspace,
        &prompt,
        &parent_cancel,
    )
}

/// Launch a new (non-resumed) runtime for any client: try a persisted
/// session first, fall back to an in-memory one with a warning.
fn launch_fresh<C: LlmClient + Clone + 'static>(
    client: C,
    backend_label: String,
    config: &AgentConfig,
    workspace: PathBuf,
    prompt: &str,
    parent_cancel: &CancellationToken,
) -> LaunchedRuntime {
    let client = Arc::new(client);
    let (tools, subagent_manager, job_store, plan_mode, shutdown) =
        build_parent_tools(Arc::clone(&client), config, &workspace, parent_cancel);

    if let Some((runtime, session_id)) =
        try_persisted_runtime((*client).clone(), tools, workspace.clone(), config, prompt)
    {
        let runtime = attach_workspace_helpers(runtime, &workspace);
        return LaunchedRuntime {
            handle: Arc::new(runtime),
            backend_label,
            session_id: Some(session_id.as_str().to_string()),
            subagent_manager,
            job_store,
            plan_mode,
            stop_hook: shutdown,
        };
    }

    eprintln!("warning: session persistence unavailable; this session will not be saved");
    let (tools, subagent_manager, job_store, plan_mode, shutdown) =
        build_parent_tools(Arc::clone(&client), config, &workspace, parent_cancel);
    let runtime = attach_workspace_helpers(
        AgentRuntime::with_system_prompt((*client).clone(), tools, prompt, config.clone(), false),
        &workspace,
    );
    LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label,
        session_id: None,
        subagent_manager,
        job_store,
        plan_mode,
        stop_hook: shutdown,
    }
}

fn launch_resumed(
    config: &AgentConfig,
    mut record: SessionRecord,
    parent_cancel: &CancellationToken,
) -> LaunchedRuntime {
    let workspace = record.workspace.clone();
    let store = match JsonSessionStore::for_workspace(&workspace) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("session store unavailable: {error}");
            return launch_runtime(config, workspace, None);
        }
    };
    record.config = ConfigSnapshot::from(config);
    record.touch();
    if let Err(error) = store.save(&record) {
        eprintln!("failed to refresh session config snapshot: {error}");
    }

    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        let client = Arc::new(client);
        let (tools, subagent_manager, job_store, plan_mode, shutdown) =
            build_parent_tools(Arc::clone(&client), config, &workspace, parent_cancel);
        let runtime = attach_workspace_helpers(
            AgentRuntime::from_session_record(
                (*client).clone(),
                tools,
                record.clone(),
                store,
                config.clone(),
            ),
            &workspace,
        );
        return LaunchedRuntime {
            handle: Arc::new(runtime),
            backend_label: format!("DeepSeek {} (resumed)", config.model),
            session_id: Some(record.id.as_str().to_string()),
            subagent_manager,
            job_store,
            plan_mode,
            stop_hook: shutdown,
        };
    }

    let client = Arc::new(EchoClient);
    let (tools, subagent_manager, job_store, plan_mode, shutdown) =
        build_parent_tools(Arc::clone(&client), config, &workspace, parent_cancel);
    let runtime = attach_workspace_helpers(
        AgentRuntime::from_session_record(EchoClient, tools, record.clone(), store, config.clone()),
        &workspace,
    );
    LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label: "offline echo (resumed)".to_string(),
        session_id: Some(record.id.as_str().to_string()),
        subagent_manager,
        job_store,
        plan_mode,
        stop_hook: shutdown,
    }
}

fn build_parent_tools<C: LlmClient + Clone + 'static>(
    client: Arc<C>,
    config: &AgentConfig,
    workspace: &Path,
    parent_cancel: &CancellationToken,
) -> (
    ToolRegistry,
    SharedSubAgentManager,
    JobStore,
    PlanMode,
    Box<dyn Fn() + Send + Sync>,
) {
    let bootstrap = RuntimeBootstrap::load(workspace, None);
    let (mut registry, job_store) = build_tool_registry(workspace);
    let extensions = attach_agent_extensions(
        &mut registry,
        client,
        config.clone(),
        workspace.to_path_buf(),
        parent_cancel.clone(),
        &bootstrap,
    );
    let shutdown: Box<dyn Fn() + Send + Sync> = Box::new({
        let extensions = Arc::clone(&extensions);
        move || extensions.cancel_all_running()
    });
    (
        registry,
        extensions.subagent_manager(),
        job_store,
        extensions.plan_mode(),
        shutdown,
    )
}

fn try_persisted_runtime<C: LlmClient + 'static>(
    client: C,
    tools: ToolRegistry,
    workspace: PathBuf,
    config: &AgentConfig,
    system_prompt: &str,
) -> Option<(AgentRuntime<C>, SessionId)> {
    let store = JsonSessionStore::for_workspace(&workspace).ok()?;
    let record = SessionRecord::new(workspace, config, system_prompt);
    let session_id = record.id.clone();
    store.save(&record).ok()?;
    Some((
        AgentRuntime::from_session_record(client, tools, record, store, config.clone()),
        session_id,
    ))
}

fn attach_workspace_helpers<C: LlmClient + 'static>(
    runtime: AgentRuntime<C>,
    workspace: &Path,
) -> AgentRuntime<C> {
    runtime
        .with_checkpoints(workspace.to_path_buf())
        .with_diagnostics(workspace.to_path_buf())
}
