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

mod checkpoints;
mod compaction_flow;
mod diagnostics;
mod event;
mod handle;
mod persistence;
mod state;
mod streaming;
mod telemetry;

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};

use crate::auto_mode::resolve_turn_route;
use crate::checkpoint::CheckpointStore;
use crate::client::LlmClient;
use crate::compaction::{estimate_token_count, stable_prefix_fingerprint};
use crate::config::AgentConfig;
use crate::event::AgentEvent;
use crate::lsp::{LspManager, is_edit_tool, render_blocks, summarize_blocks};
use crate::message::Message;
use crate::model::{ChatRequest, ToolCallFunctionPayload, ToolCallPayload, Usage};
use crate::model_registry::ModelRegistry;
use crate::pricing::CostEstimate;
use crate::session::Session;
use crate::session_store::TurnRecord;
use crate::tool::{
    ApprovalDecision, ApprovalRequest, ToolCall, ToolCallAccumulator, ToolError, ToolRegistry,
    ToolResult, ToolResultStatus, ToolRunOutcome,
};
use diagnostics::append_diagnostics;
use event::emit;
pub use event::{RuntimeEvent, RuntimeEventReceiver};
pub use handle::AgentRuntimeHandle;
use state::{PendingToolCall, Persistence, RuntimeState};

/// Agent runtime tying together [`LlmClient`], [`ToolRegistry`], and [`Session`].
///
/// Cheap to clone: state is behind an [`Arc`]/[`Mutex`].
pub struct AgentRuntime<C: LlmClient + 'static> {
    client: Arc<C>,
    config: AgentConfig,
    registry: ModelRegistry,
    is_subagent: bool,
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
            config: self.config.clone(),
            registry: self.registry.clone(),
            is_subagent: self.is_subagent,
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
        Self::with_config(client, tools, AgentConfig::default())
    }

    pub fn with_config(client: C, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            client: Arc::new(client),
            config: config.clone(),
            registry: ModelRegistry::default(),
            is_subagent: false,
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState::default())),
            checkpoints: None,
            workspace: None,
            lsp: None,
            persistence: None,
        }
    }

    pub fn with_system_prompt(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        config: AgentConfig,
        is_subagent: bool,
    ) -> Self {
        Self::with_system_prompt_flags(client, tools, system, config, is_subagent)
    }

    fn with_system_prompt_flags(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        config: AgentConfig,
        is_subagent: bool,
    ) -> Self {
        let mut session = Session::new();
        let system_text = system.into();
        if !system_text.is_empty() {
            session.push(Message::system(system_text));
        }
        Self {
            client: Arc::new(client),
            config: config.clone(),
            registry: ModelRegistry::default(),
            is_subagent,
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                pending: None,
                current_turn: None,
                last_prefix_hash: None,
                session_cost: CostEstimate::default(),
                current_prompt: None,
            })),
            checkpoints: None,
            workspace: None,
            lsp: None,
            persistence: None,
        }
    }

    /// Shut down background resources such as spawned LSP servers.
    pub async fn shutdown(&self) {
        self.persist().await;
        if let Some(lsp) = self.lsp.as_ref() {
            lsp.shutdown_all().await;
        }
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
            state.current_turn = Some(TurnRecord::new(prompt.clone()));
            state.current_prompt = Some(prompt);
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
        let user_prompt = {
            let state = self.state.lock().await;
            state.current_prompt.clone().unwrap_or_default()
        };
        let mut route =
            resolve_turn_route(&self.config, &self.registry, &user_prompt, self.is_subagent);

        if self.maybe_compact(&route.effective_model, tx).await {
            // compaction event already emitted; continue with trimmed history
        }

        loop {
            let (messages, prefix_hash) = {
                let state = self.state.lock().await;
                let messages = state.session.messages().to_vec();
                let prefix_hash = stable_prefix_fingerprint(&messages);
                (messages, prefix_hash)
            };

            let estimated_context_tokens = estimate_token_count(&messages);

            let mut request = ChatRequest::streaming(route.effective_model.clone(), messages)
                .with_tools(self.tools.chat_tools());
            if let Some(effort) = route.effective_effort.as_api_value() {
                request = request.with_reasoning_effort(effort);
            }

            let mut stream = match self.stream_with_fallback(&mut route, request).await {
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
                let telemetry = self.build_turn_telemetry(
                    &route,
                    usage.as_ref(),
                    prefix_hash,
                    estimated_context_tokens,
                );
                self.finish_turn(usage.clone()).await;
                emit(
                    tx,
                    RuntimeEvent::TurnFinished {
                        usage: last_usage,
                        telemetry: Some(telemetry),
                    },
                );
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
        if result.status == ToolResultStatus::Success
            && is_edit_tool(&call.name)
            && let Some(lsp) = self.lsp.as_ref()
        {
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

#[cfg(test)]
#[path = "runtime/integration_tests.rs"]
mod tests;
