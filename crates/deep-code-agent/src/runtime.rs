//! Agent runtime: orchestrates the model loop, tool calls, and approvals.
//!
//! The runtime owns the [`Session`], an [`LlmClient`], and a [`ToolRegistry`],
//! and produces [`RuntimeEvent`]s for UIs. UIs are expected to render events
//! and forward approval decisions back via [`AgentRuntime::submit_approval`].
//!
//! Design notes
//! - [`AgentEvent`] is intentionally kept narrow (provider-stream only). The
//!   runtime synthesizes higher-level events such as approval requests and
//!   tool results into [`RuntimeEvent`].
//! - Multi tool-call turns are not supported yet: the runtime emits a
//!   `RuntimeEvent::Error` if the model produces more than one tool call in
//!   one turn. The single-call path is enough for the 03 milestone.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};

use crate::checkpoint::{CheckpointId, CheckpointStore};
use crate::client::LlmClient;
use crate::event::AgentEvent;
use crate::lsp::{LspConfig, LspManager, is_edit_tool, render_blocks, summarize_blocks};
use crate::message::Message;
use crate::model::{ChatRequest, ToolCallFunctionPayload, ToolCallPayload, Usage};
use crate::session::Session;
use crate::session_store::{JsonSessionStore, SessionId, SessionRecord, SessionStore, TurnRecord};
use crate::tool::{
    ApprovalDecision, ApprovalRequest, ToolCall, ToolCallAccumulator, ToolError, ToolRegistry,
    ToolResult, ToolResultStatus, ToolRunOutcome,
};

/// Events the agent runtime produces for UIs.
///
/// These are higher level than [`AgentEvent`]: approval requests and tool
/// results are emitted by the runtime, never by an [`LlmClient`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// Forwarded provider event (text/reasoning/tool-call-delta).
    Provider(AgentEvent),
    /// Runtime is requesting human approval for a tool call.
    ApprovalRequired { request: ApprovalRequest },
    /// A tool finished (executed, denied, or failed) and its result has been
    /// recorded in the session.
    ToolResult { result: ToolResult },
    /// Current turn finished cleanly (no further provider activity).
    TurnFinished { usage: Option<Usage> },
    /// Workspace snapshot stored under `.deep-code/checkpoints/`.
    CheckpointCreated {
        id: CheckpointId,
        label: String,
    },
    /// Workspace restored from a checkpoint (via runtime or UI command).
    WorkspaceRestored { id: CheckpointId },
    /// Post-edit LSP diagnostics were collected for one or more files.
    DiagnosticsUpdated {
        summary: String,
        rendered: String,
    },
    /// Runtime-level error. Terminal for the current turn.
    Error { message: String },
}

pub type RuntimeEventReceiver = mpsc::UnboundedReceiver<RuntimeEvent>;

/// Object-safe handle so that callers (UIs, tests) can hold heterogeneous
/// runtimes (DeepSeek, offline echo, scripted, ...) behind a `Box<dyn ...>`.
///
/// Methods here return owned futures so the trait stays object-safe even
/// though [`LlmClient`] is not.
pub trait AgentRuntimeHandle: Send + Sync {
    fn submit_user(
        &self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>;

    fn submit_approval(
        &self,
        decision: ApprovalDecision,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>;

    fn session_messages(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Message>> + Send + '_>>;

    fn restore_checkpoint(
        &self,
        id: CheckpointId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>;

    fn session_id(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SessionId>> + Send + '_>>;

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

impl<C: LlmClient + 'static> AgentRuntimeHandle for AgentRuntime<C> {
    fn submit_user(
        &self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>
    {
        Box::pin(AgentRuntime::submit_user(self, prompt))
    }

    fn submit_approval(
        &self,
        decision: ApprovalDecision,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>
    {
        Box::pin(AgentRuntime::submit_approval(self, decision))
    }

    fn session_messages(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Message>> + Send + '_>> {
        Box::pin(AgentRuntime::session_messages(self))
    }

    fn restore_checkpoint(
        &self,
        id: CheckpointId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>
    {
        Box::pin(AgentRuntime::restore_checkpoint(self, id))
    }

    fn session_id(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SessionId>> + Send + '_>> {
        Box::pin(AgentRuntime::session_id(self))
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(AgentRuntime::shutdown(self))
    }
}

/// Internal: the runtime can be in one of these states between [`RuntimeEvent`]
/// emissions. Kept private so callers cannot poke at it.
#[derive(Debug, Default)]
struct RuntimeState {
    session: Session,
    pending: Option<PendingToolCall>,
    current_turn: Option<TurnRecord>,
}

struct Persistence {
    store: Arc<JsonSessionStore>,
    record: Arc<Mutex<SessionRecord>>,
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    call: ToolCall,
}

/// Agent runtime tying together [`LlmClient`], [`ToolRegistry`], and [`Session`].
///
/// Cheap to clone: state is behind an [`Arc`]/[`Mutex`].
pub struct AgentRuntime<C: LlmClient + 'static> {
    client: Arc<C>,
    tools: Arc<ToolRegistry>,
    state: Arc<Mutex<RuntimeState>>,
    checkpoints: Option<Arc<CheckpointStore>>,
    workspace: Option<PathBuf>,
    lsp: Option<Arc<LspManager>>,
    persistence: Option<Arc<Persistence>>,
}

// Manual `Clone` to avoid the auto-derived `where C: Clone` bound that the
// compiler would otherwise add. `client` is already an `Arc<C>`, so cloning
// the runtime never requires cloning `C` itself.
impl<C: LlmClient + 'static> Clone for AgentRuntime<C> {
    fn clone(&self) -> Self {
        Self {
            client: Arc::clone(&self.client),
            tools: Arc::clone(&self.tools),
            state: Arc::clone(&self.state),
            checkpoints: self.checkpoints.clone(),
            workspace: self.workspace.clone(),
            lsp: self.lsp.clone(),
            persistence: self.persistence.clone(),
        }
    }
}

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub fn new(client: C, tools: ToolRegistry) -> Self {
        Self {
            client: Arc::new(client),
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState::default())),
            checkpoints: None,
            workspace: None,
            lsp: None,
            persistence: None,
        }
    }

    pub fn with_system_prompt(client: C, tools: ToolRegistry, system: impl Into<String>) -> Self {
        let mut session = Session::new();
        session.push(Message::system(system));
        Self {
            client: Arc::new(client),
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                pending: None,
                current_turn: None,
            })),
            checkpoints: None,
            workspace: None,
            lsp: None,
            persistence: None,
        }
    }

    /// Create a runtime backed by a new on-disk session in the workspace.
    pub fn with_new_session(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        workspace: impl Into<PathBuf>,
        config: &crate::config::AgentConfig,
    ) -> Result<Self, crate::session_store::SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let record = SessionRecord::new(workspace.clone(), config, system);
        store.save(&record)?;
        Ok(Self::from_session_record(client, tools, record, store))
    }

    /// Resume a runtime from a previously saved session record.
    #[must_use]
    pub fn from_session_record(
        client: C,
        tools: ToolRegistry,
        record: SessionRecord,
        store: JsonSessionStore,
    ) -> Self {
        let workspace = record.workspace.clone();
        let session = Session::from_messages(record.messages.clone());
        Self {
            client: Arc::new(client),
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                pending: None,
                current_turn: None,
            })),
            checkpoints: None,
            workspace: Some(workspace.clone()),
            lsp: None,
            persistence: Some(Arc::new(Persistence {
                store: Arc::new(store),
                record: Arc::new(Mutex::new(record)),
            })),
        }
    }

    /// Attach session persistence to an existing runtime, creating a new record.
    pub fn enable_persistence(
        mut self,
        workspace: impl Into<PathBuf>,
        config: &crate::config::AgentConfig,
        system_prompt: impl Into<String>,
    ) -> Result<Self, crate::session_store::SessionStoreError> {
        let workspace = workspace.into();
        let store = JsonSessionStore::for_workspace(&workspace)?;
        let mut record = SessionRecord::new(workspace.clone(), config, system_prompt);
        {
            let state = self.state.try_lock().map_err(|_| {
                crate::session_store::SessionStoreError::Io {
                    message: "runtime state is busy".to_string(),
                }
            })?;
            record.messages = state.session.messages().to_vec();
        }
        store.save(&record)?;
        self.workspace = Some(workspace);
        self.persistence = Some(Arc::new(Persistence {
            store: Arc::new(store),
            record: Arc::new(Mutex::new(record)),
        }));
        Ok(self)
    }

    #[must_use]
    pub fn persistence_enabled(&self) -> bool {
        self.persistence.is_some()
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        let persistence = self.persistence.as_ref()?;
        Some(persistence.record.lock().await.id.clone())
    }

    async fn persist(&self) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let messages = self.state.lock().await.session.messages().to_vec();
        let mut record = persistence.record.lock().await;
        record.messages = messages;
        record.touch();
        if let Err(error) = persistence.store.save(&record) {
            eprintln!("session save failed: {error}");
        }
    }

    async fn finish_turn(&self, usage: Option<Usage>) {
        let mut state = self.state.lock().await;
        if let Some(mut turn) = state.current_turn.take() {
            turn.finish(usage);
            drop(state);
            if let Some(persistence) = self.persistence.as_ref() {
                let mut record = persistence.record.lock().await;
                record.turns.push(turn);
                record.touch();
                if let Err(error) = persistence.store.save(&record) {
                    eprintln!("session save failed: {error}");
                }
            }
        } else {
            drop(state);
        }
        self.persist().await;
    }

    async fn abort_turn(&self) {
        self.finish_turn(None).await;
    }

    async fn finalize_orphan_turn(&self) {
        let has_open = self.state.lock().await.current_turn.is_some();
        if has_open {
            self.finish_turn(None).await;
        }
    }

    /// Enable automatic before/after turn snapshots for the given workspace root.
    ///
    /// If checkpoint storage cannot be created, checkpoints stay disabled and the
    /// runtime is still returned.
    #[must_use]
    pub fn with_checkpoints(mut self, workspace: impl Into<PathBuf>) -> Self {
        match CheckpointStore::new(workspace) {
            Ok(store) => self.checkpoints = Some(Arc::new(store)),
            Err(error) => eprintln!("checkpoints disabled: {error}"),
        }
        self
    }

    /// Enable post-edit LSP diagnostics for the given workspace root.
    #[must_use]
    pub fn with_diagnostics(mut self, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        self.workspace = Some(workspace.clone());
        self.lsp = Some(Arc::new(LspManager::new(LspConfig::default(), workspace)));
        self
    }

    /// Enable post-edit LSP diagnostics with explicit config.
    #[must_use]
    pub fn with_diagnostics_config(
        mut self,
        workspace: impl Into<PathBuf>,
        config: LspConfig,
    ) -> Self {
        let workspace = workspace.into();
        self.workspace = Some(workspace.clone());
        self.lsp = Some(Arc::new(LspManager::new(config, workspace)));
        self
    }

    #[must_use]
    pub fn diagnostics_enabled(&self) -> bool {
        self.lsp
            .as_ref()
            .is_some_and(|manager| manager.config().enabled)
    }

    /// Shut down background resources such as spawned LSP servers.
    pub async fn shutdown(&self) {
        self.persist().await;
        if let Some(lsp) = self.lsp.as_ref() {
            lsp.shutdown_all().await;
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_lsp_manager(mut self, workspace: PathBuf, manager: LspManager) -> Self {
        self.workspace = Some(workspace);
        self.lsp = Some(Arc::new(manager));
        self
    }

    /// Restore workspace files from a checkpoint id.
    pub async fn restore_checkpoint(&self, id: CheckpointId) -> Result<(), ToolError> {
        let store = self
            .checkpoints
            .as_ref()
            .ok_or_else(|| ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: "checkpoints are not enabled on this runtime".to_string(),
            })?;
        store.restore(&id)
    }

    #[must_use]
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.is_some()
    }

    /// Start a new turn from a user prompt. Returns a receiver that yields
    /// [`RuntimeEvent`]s until either the turn finishes or an approval is
    /// required. After approval, call [`submit_approval`] to resume.
    pub async fn submit_user(&self, prompt: impl Into<String>) -> RuntimeEventReceiver {
        self.begin_turn(prompt).await;
        self.drive_turn().await
    }

    /// Record a user prompt and start turn bookkeeping without spawning the loop.
    pub async fn begin_turn(&self, prompt: impl Into<String>) {
        self.finalize_orphan_turn().await;
        let prompt = prompt.into();
        {
            let mut state = self.state.lock().await;
            state.session.push(Message::user(&prompt));
            state.pending = None;
            state.current_turn = Some(TurnRecord::new(prompt));
        }
        self.persist().await;
    }

    /// Spawn the model/tool loop for the current turn.
    pub async fn drive_turn(&self) -> RuntimeEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        self.snapshot_turn("before_turn", &tx);
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.run_loop(&tx).await;
        });
        rx
    }

    /// Resolve a pending tool approval and resume the loop.
    ///
    /// If there is no pending approval, returns an error event on the stream.
    pub async fn submit_approval(&self, decision: ApprovalDecision) -> RuntimeEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = {
            let mut state = self.state.lock().await;
            state.pending.take()
        };

        let Some(pending) = pending else {
            let _ = tx.send(RuntimeEvent::Error {
                message: "no pending tool approval".to_string(),
            });
            return rx;
        };

        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.handle_approval(pending, decision, &tx).await;
        });
        rx
    }

    /// Drive the model/tool loop until either the turn finishes or an
    /// approval is required. All paths emit a terminal [`RuntimeEvent`]
    /// (`TurnFinished`, `ApprovalRequired`, or `Error`) before returning.
    async fn run_loop(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        loop {
            let request = {
                let state = self.state.lock().await;
                ChatRequest::streaming(
                    self.client.model().to_string(),
                    state.session.messages().to_vec(),
                )
                .with_tools(self.tools.chat_tools())
            };

            let mut stream = match self.client.stream_chat(request).await {
                Ok(stream) => stream,
                Err(error) => {
                    emit(
                        tx,
                        RuntimeEvent::Error {
                            message: error.to_string(),
                        },
                    );
                    self.abort_turn().await;
                    return;
                }
            };

            let mut accumulator = ToolCallAccumulator::default();
            let mut text_buffer = String::new();
            let mut last_usage: Option<Usage> = None;
            let mut had_error = false;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        text_buffer.push_str(&text);
                        emit(tx, RuntimeEvent::Provider(AgentEvent::TextDelta { text }));
                    }
                    Ok(AgentEvent::ReasoningDelta { text }) => {
                        emit(
                            tx,
                            RuntimeEvent::Provider(AgentEvent::ReasoningDelta { text }),
                        );
                    }
                    Ok(AgentEvent::ToolCallDelta { delta }) => {
                        let forwarded = delta.clone();
                        accumulator.push_delta(delta);
                        emit(
                            tx,
                            RuntimeEvent::Provider(AgentEvent::ToolCallDelta { delta: forwarded }),
                        );
                    }
                    Ok(AgentEvent::Done { usage }) => {
                        last_usage = usage;
                    }
                    Ok(AgentEvent::Error { message }) => {
                        emit(tx, RuntimeEvent::Error { message });
                        had_error = true;
                    }
                    Err(error) => {
                        emit(
                            tx,
                            RuntimeEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        had_error = true;
                    }
                }
            }

            if had_error {
                self.abort_turn().await;
                return;
            }

            let calls = match accumulator.finish() {
                Ok(calls) => calls,
                Err(error) => {
                    emit(tx, runtime_error_from_tool_error(error));
                    self.abort_turn().await;
                    return;
                }
            };

            if calls.is_empty() {
                let mut state = self.state.lock().await;
                state.session.push(Message::assistant(text_buffer));
                drop(state);
                self.snapshot_turn("after_turn", tx);
                let usage = last_usage.clone();
                self.finish_turn(usage).await;
                emit(tx, RuntimeEvent::TurnFinished { usage: last_usage });
                return;
            }

            if calls.len() > 1 {
                emit(
                    tx,
                    RuntimeEvent::Error {
                        message: format!(
                            "multi tool call turns are not supported yet (got {} calls)",
                            calls.len()
                        ),
                    },
                );
                self.abort_turn().await;
                return;
            }

            let call = calls.into_iter().next().expect("exactly one tool call");
            let payload = tool_call_payload(&call);

            {
                let mut state = self.state.lock().await;
                state.session.push(Message::assistant_with_tool_calls(
                    text_buffer,
                    vec![payload],
                ));
            }
            self.persist().await;

            match self.tools.run_tool_call(call.clone(), None) {
                Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                    {
                        let mut state = self.state.lock().await;
                        state.pending = Some(PendingToolCall { call });
                    }
                    emit(tx, RuntimeEvent::ApprovalRequired { request });
                    return;
                }
                Ok(ToolRunOutcome::Result { result }) => {
                    self.record_tool_result(&call, result, tx).await;
                    // Loop again: feed tool result back into the next chat turn.
                    continue;
                }
                Err(error) => {
                    emit(tx, runtime_error_from_tool_error(error));
                    self.abort_turn().await;
                    return;
                }
            }
        }
    }

    async fn handle_approval(
        &self,
        pending: PendingToolCall,
        decision: ApprovalDecision,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let outcome = self
            .tools
            .run_tool_call(pending.call.clone(), Some(decision));
        match outcome {
            Ok(ToolRunOutcome::Result { result }) => {
                self.record_tool_result(&pending.call, result, tx).await;
            }
            Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                {
                    let mut state = self.state.lock().await;
                    state.pending = Some(pending);
                }
                emit(tx, RuntimeEvent::ApprovalRequired { request });
                return;
            }
            Err(error) => {
                emit(tx, runtime_error_from_tool_error(error));
                self.abort_turn().await;
                return;
            }
        }

        // Approved (or denied) call recorded; resume the loop to feed the
        // tool result into the next chat turn.
        self.run_loop(tx).await;
    }

    async fn record_tool_result(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if result.status == ToolResultStatus::Success && is_edit_tool(&call.name) {
            if let Some(lsp) = self.lsp.as_ref() {
                let blocks = lsp.collect_for_edit(&call.name, &call.arguments).await;
                if !blocks.is_empty() {
                    let rendered = render_blocks(&blocks);
                    let summary = summarize_blocks(&blocks);
                    result.content = append_diagnostics(&result.content, &rendered);
                    emit(
                        tx,
                        RuntimeEvent::DiagnosticsUpdated {
                            summary: summary.clone(),
                            rendered,
                        },
                    );
                }
            }
        }

        {
            let mut state = self.state.lock().await;
            state.session.push(result.to_message());
            if let Some(turn) = state.current_turn.as_mut() {
                turn.tool_results.push(result.clone());
            }
        }
        self.persist().await;
        emit(tx, RuntimeEvent::ToolResult { result });
    }

    fn snapshot_turn(&self, label: &str, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let Some(store) = self.checkpoints.as_ref() else {
            return;
        };
        match store.snapshot(label) {
            Ok(id) => emit(
                tx,
                RuntimeEvent::CheckpointCreated {
                    id,
                    label: label.to_string(),
                },
            ),
            Err(error) => {
                eprintln!("checkpoint snapshot '{label}' failed: {error}");
            }
        }
    }

    /// Snapshot the current message history. Mostly for tests/debugging.
    pub async fn session_messages(&self) -> Vec<Message> {
        self.state.lock().await.session.messages().to_vec()
    }

    /// Resolve a pending tool call for a background sub-agent.
    ///
    /// Inherits the runtime [`ToolRegistry`] execution policy: only tools that
    /// are allowed without human approval are auto-approved. Write tools and
    /// shell commands that would normally require parent approval are denied.
    pub fn subagent_approval_decision(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let call = ToolCall::new(
            request.call_id.clone(),
            request.tool_name.clone(),
            request.arguments.clone(),
        );
        let plan = self.tools.evaluate_tool(&call);
        if plan.denied_reason().is_some() || plan.requires_approval {
            ApprovalDecision::Denied
        } else {
            ApprovalDecision::Approved
        }
    }
}

fn emit(tx: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
    let _ = tx.send(event);
}

fn tool_call_payload(call: &ToolCall) -> ToolCallPayload {
    // Compact form keeps history small and matches typical OpenAI-style
    // assistant payloads. We don't try to preserve the exact bytes the model
    // produced because we already parsed them through `ToolCallAccumulator`.
    let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string());
    ToolCallPayload {
        id: call.id.clone(),
        call_type: "function".to_string(),
        function: ToolCallFunctionPayload {
            name: call.name.clone(),
            arguments,
        },
    }
}

fn runtime_error_from_tool_error(error: ToolError) -> RuntimeEvent {
    RuntimeEvent::Error {
        message: error.to_string(),
    }
}

fn append_diagnostics(content: &str, rendered: &str) -> String {
    if rendered.is_empty() {
        content.to_string()
    } else if content.is_empty() {
        rendered.to_string()
    } else {
        format!("{content}\n\n{rendered}")
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Mutex;

    use async_stream::try_stream;
    use futures_core::Stream;

    use super::*;
    use crate::client::AgentEventStream;
    use crate::error::AgentResult;
    use crate::event::AgentEvent;
    use crate::model::{FunctionCallDelta, ToolCallDelta};
    use crate::tool::{MockEchoTool, ToolRegistry, ToolResultStatus};

    #[test]
    fn append_diagnostics_joins_blocks() {
        assert_eq!(
            append_diagnostics("{\"path\":\"a.rs\"}", "<diagnostics file=\"a.rs\">\n</diagnostics>"),
            "{\"path\":\"a.rs\"}\n\n<diagnostics file=\"a.rs\">\n</diagnostics>"
        );
    }

    /// Scripted client: replays a pre-recorded sequence of provider events for
    /// each successive call to `stream_chat`.
    struct ScriptedClient {
        scripts: Mutex<Vec<Vec<AgentEvent>>>,
    }

    impl ScriptedClient {
        fn new(scripts: Vec<Vec<AgentEvent>>) -> Self {
            Self {
                scripts: Mutex::new(scripts),
            }
        }
    }

    impl LlmClient for ScriptedClient {
        fn provider_name(&self) -> &'static str {
            "scripted"
        }

        fn model(&self) -> &str {
            "scripted-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let events = {
                let mut scripts = self.scripts.lock().unwrap();
                if scripts.is_empty() {
                    Vec::new()
                } else {
                    scripts.remove(0)
                }
            };

            let stream = try_stream! {
                for event in events {
                    yield event;
                }
            };
            let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> =
                Box::pin(stream);
            Ok(stream)
        }
    }

    fn tool_call_delta(id: &str, name: &str, arguments: &str) -> ToolCallDelta {
        ToolCallDelta {
            index: Some(0),
            id: Some(id.to_string()),
            call_type: Some("function".to_string()),
            function: Some(FunctionCallDelta {
                name: Some(name.to_string()),
                arguments: Some(arguments.to_string()),
            }),
        }
    }

    async fn drain(rx: &mut RuntimeEventReceiver) -> Vec<RuntimeEvent> {
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn approve_path_feeds_tool_result_into_next_turn() {
        let client = ScriptedClient::new(vec![
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "thanks".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ]);
        let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

        let mut rx = runtime.submit_user("please echo").await;
        let first = drain(&mut rx).await;
        assert!(matches!(
            first.last(),
            Some(RuntimeEvent::ApprovalRequired { .. })
        ));

        let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
        let second = drain(&mut rx).await;

        let mut saw_tool_result = false;
        let mut saw_finish = false;
        for event in &second {
            match event {
                RuntimeEvent::ToolResult { result } => {
                    assert_eq!(result.status, ToolResultStatus::Success);
                    assert_eq!(result.content, "mock_echo: hi");
                    saw_tool_result = true;
                }
                RuntimeEvent::TurnFinished { .. } => saw_finish = true,
                _ => {}
            }
        }
        assert!(saw_tool_result, "expected ToolResult event after approval");
        assert!(saw_finish, "expected TurnFinished after second turn");

        let messages = runtime.session_messages().await;
        // Expect: user, assistant(tool_calls), tool, assistant("thanks").
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].id, "call_1");
        assert_eq!(messages[2].role, crate::message::Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].content, "thanks");
    }

    #[tokio::test]
    async fn deny_path_records_denied_tool_message_and_continues() {
        let client = ScriptedClient::new(vec![
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "ok".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ]);
        let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

        let mut rx = runtime.submit_user("please echo").await;
        drain(&mut rx).await;

        let mut rx = runtime.submit_approval(ApprovalDecision::Denied).await;
        let events = drain(&mut rx).await;

        let denied = events.iter().find_map(|event| match event {
            RuntimeEvent::ToolResult { result } => Some(result),
            _ => None,
        });
        let denied = denied.expect("expected ToolResult on deny path");
        assert_eq!(denied.status, ToolResultStatus::Denied);

        let messages = runtime.session_messages().await;
        assert!(
            messages
                .iter()
                .any(|m| matches!(m.role, crate::message::Role::Tool)
                    && m.content.contains("denied"))
        );
    }

    #[tokio::test]
    async fn plain_response_yields_assistant_message_and_finish() {
        let client = ScriptedClient::new(vec![vec![
            AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]]);
        let runtime = AgentRuntime::new(client, ToolRegistry::default());

        let mut rx = runtime.submit_user("hi").await;
        let events = drain(&mut rx).await;

        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::TurnFinished { .. })
        ));
        let messages = runtime.session_messages().await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "hello");
        assert!(messages[1].tool_calls.is_empty());
    }

    #[tokio::test]
    async fn turn_snapshots_emit_checkpoint_events() {
        let workspace = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]]);
        let runtime = AgentRuntime::new(client, ToolRegistry::default())
            .with_checkpoints(workspace.path());

        let mut rx = runtime.submit_user("hi").await;
        let events = drain(&mut rx).await;

        let before = events.iter().find_map(|event| match event {
            RuntimeEvent::CheckpointCreated { id, label } if label == "before_turn" => Some(id.0.clone()),
            _ => None,
        });
        let after = events.iter().find_map(|event| match event {
            RuntimeEvent::CheckpointCreated { id, label } if label == "after_turn" => Some(id.0.clone()),
            _ => None,
        });
        assert!(before.is_some(), "expected before_turn checkpoint");
        assert!(after.is_some(), "expected after_turn checkpoint");
    }

    #[tokio::test]
    async fn submit_approval_without_pending_emits_error() {
        let client = ScriptedClient::new(vec![]);
        let runtime = AgentRuntime::new(client, ToolRegistry::default());

        let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
        let events = drain(&mut rx).await;
        assert!(matches!(events.first(), Some(RuntimeEvent::Error { .. })));
    }

    #[tokio::test]
    async fn write_file_appends_lsp_diagnostics_to_session() {
        use std::sync::Arc;

        use async_trait::async_trait;

        use crate::lsp::{Diagnostic, DiagnosticRange, Language, LspConfig, LspManager, LspTransport, Severity};
        use crate::workspace_tools::workspace_tool_registry;

        struct FakeTransport {
            items: Vec<Diagnostic>,
        }

        #[async_trait]
        impl LspTransport for FakeTransport {
            async fn diagnostics_for(
                &self,
                _path: &std::path::Path,
                _text: &str,
                _wait: std::time::Duration,
            ) -> anyhow::Result<Vec<Diagnostic>> {
                Ok(self.items.clone())
            }

            async fn shutdown(&self) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
        manager
            .install_test_transport(
                Language::Rust,
                Arc::new(FakeTransport {
                    items: vec![Diagnostic {
                        file: dir.path().join("broken.rs"),
                        range: DiagnosticRange {
                            start_line: 1,
                            start_column: 1,
                            end_line: 1,
                            end_column: 2,
                        },
                        severity: Severity::Error,
                        message: "syntax error".to_string(),
                        source: None,
                        code: None,
                    }],
                }),
            )
            .await;

        let args = r#"{"path":"broken.rs","content":"fn main() {"}"#;
        let client = ScriptedClient::new(vec![
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta("call_1", "write_file", args),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "fixed".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ]);
        let runtime = AgentRuntime::new(client, workspace_tool_registry(dir.path()).unwrap())
            .with_lsp_manager(dir.path().to_path_buf(), manager);

        let mut rx = runtime.submit_user("write broken rust").await;
        drain(&mut rx).await;

        let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
        let events = drain(&mut rx).await;

        let tool_result = events.iter().find_map(|event| match event {
            RuntimeEvent::ToolResult { result } => Some(result),
            _ => None,
        });
        let tool_result = tool_result.expect("tool result after approval");
        assert!(tool_result.content.contains("<diagnostics file="));
        assert!(tool_result.content.contains("syntax error"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::DiagnosticsUpdated { .. }))
        );

        let messages = runtime.session_messages().await;
        let tool_message = messages
            .iter()
            .find(|message| matches!(message.role, crate::message::Role::Tool))
            .expect("tool message");
        assert!(tool_message.content.contains("<diagnostics file="));
    }

    #[tokio::test]
    async fn persistence_saves_messages_and_turns() {
        let workspace = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![vec![
            AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]]);
        let runtime = AgentRuntime::with_new_session(
            client,
            ToolRegistry::default(),
            "system",
            workspace.path(),
            &crate::config::AgentConfig::default(),
        )
        .unwrap();

        let session_id = runtime.session_id().await.expect("session id");
        let mut rx = runtime.submit_user("hi").await;
        drain(&mut rx).await;
        runtime.shutdown().await;

        let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let record = store.load(&session_id).unwrap();
        assert_eq!(record.messages.len(), 3);
        assert_eq!(record.turns.len(), 1);
        assert_eq!(record.turns[0].user_prompt, "hi");
    }

    #[tokio::test]
    async fn stream_error_finalizes_open_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![vec![AgentEvent::Error {
            message: "boom".to_string(),
        }]]);
        let runtime = AgentRuntime::with_new_session(
            client,
            ToolRegistry::default(),
            "system",
            workspace.path(),
            &crate::config::AgentConfig::default(),
        )
        .unwrap();

        let session_id = runtime.session_id().await.expect("session id");
        let mut rx = runtime.submit_user("hi").await;
        drain(&mut rx).await;
        runtime.shutdown().await;

        let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let record = store.load(&session_id).unwrap();
        assert_eq!(record.turns.len(), 1);
        assert_eq!(record.turns[0].user_prompt, "hi");
        assert!(record.turns[0].finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn persistence_saves_tool_results_in_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "thanks".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ]);
        let runtime = AgentRuntime::with_new_session(
            client,
            ToolRegistry::with_mock_tools(),
            "system",
            workspace.path(),
            &crate::config::AgentConfig::default(),
        )
        .unwrap();

        let session_id = runtime.session_id().await.expect("session id");
        let mut rx = runtime.submit_user("please echo").await;
        drain(&mut rx).await;
        let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
        drain(&mut rx).await;
        runtime.shutdown().await;

        let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let record = store.load(&session_id).unwrap();
        assert_eq!(record.turns.len(), 1);
        assert_eq!(record.turns[0].user_prompt, "please echo");
        assert_eq!(record.turns[0].tool_results.len(), 1);
        assert_eq!(record.turns[0].tool_results[0].tool_name, MockEchoTool::NAME);
        assert_eq!(record.turns[0].tool_results[0].content, "mock_echo: hi");
    }

    #[tokio::test]
    async fn resumed_runtime_continues_conversation() {
        let workspace = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            vec![
                AgentEvent::TextDelta {
                    text: "hello".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "world".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ]);
        let runtime = AgentRuntime::with_new_session(
            client,
            ToolRegistry::default(),
            "system",
            workspace.path(),
            &crate::config::AgentConfig::default(),
        )
        .unwrap();

        let session_id = runtime.session_id().await.expect("session id");
        let mut rx = runtime.submit_user("first").await;
        drain(&mut rx).await;
        runtime.shutdown().await;

        let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let record = store.load(&session_id).unwrap();
        assert_eq!(record.messages.len(), 3);

        let resumed = AgentRuntime::from_session_record(
            ScriptedClient::new(vec![vec![
                AgentEvent::TextDelta {
                    text: "world".to_string(),
                },
                AgentEvent::Done { usage: None },
            ]]),
            ToolRegistry::default(),
            record,
            store,
        );
        let mut rx = resumed.submit_user("second").await;
        drain(&mut rx).await;
        let messages = resumed.session_messages().await;
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[3].content, "second");
        assert_eq!(messages[4].content, "world");
    }
}
