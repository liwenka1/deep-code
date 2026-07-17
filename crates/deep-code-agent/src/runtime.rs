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
//! - Multi tool-call turns run as an ordered batch: auto-approved calls
//!   execute immediately, approval-gated calls park the rest of the batch
//!   until the decision arrives (one approval prompt at a time).

mod checkpoints;
mod compaction_flow;
mod diagnostics;
mod event;
mod handle;
mod persistence;
mod persistence_actor;
mod state;
mod streaming;
mod telemetry;
mod tool_result;
mod turn_loop;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::checkpoint::CheckpointStore;
use crate::client::LlmClient;
use crate::config::AgentConfig;
use crate::lsp::LspManager;
use crate::message::Message;
use crate::model_registry::ModelRegistry;
use crate::pricing::CostEstimate;
use crate::session::Session;
use crate::session_store::{TurnRecord, now_ms};
use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolRegistry};
use event::emit;
pub use event::{RuntimeEvent, RuntimeEventReceiver, ToolCallId, TurnId};
pub use handle::AgentRuntimeHandle;
use state::{Persistence, RuntimeState};

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
            session.push_system(system_text);
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
                session_cache_hit_tokens: 0,
                session_cache_miss_tokens: 0,
                session_cache_savings: CostEstimate::default(),
                current_prompt: None,
                current_turn_id: None,
                cancel: CancellationToken::new(),
                session_approved: Default::default(),
                session_trusted_shell_prefixes: Default::default(),
                cascade_escalated: false,
                turn_tool_errors: 0,
                cascade_triggered_this_turn: false,
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
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.actor.flush().await;
        }
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
        let turn_id = TurnId::new();
        {
            let mut state = self.state.lock().await;
            // Interrupted tool calls need no repair here: pending exchanges
            // synthesize their placeholder at wire derivation.
            state.session.push_user(&prompt);
            state.pending = None;
            state.current_turn = Some(TurnRecord::new(prompt.clone()));
            state.current_prompt = Some(prompt);
            state.current_turn_id = Some(turn_id);
            state.cancel = CancellationToken::new();
            // Per-turn struggle counter and the "triggered this turn" flag
            // reset; the `cascade_escalated` latch intentionally persists for
            // the rest of the session.
            state.turn_tool_errors = 0;
            state.cascade_triggered_this_turn = false;
        }
        self.persist().await;
    }

    /// Spawn the model/tool loop for the current turn.
    pub async fn drive_turn(&self) -> RuntimeEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let (turn_id, prompt) = self.current_turn_context().await;
        emit(&tx, RuntimeEvent::TurnStarted { turn_id, prompt });
        self.emit_session_updated(&tx).await;
        self.snapshot_turn("before_turn", &tx).await;
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
        let (pending, cancelled) = {
            let mut state = self.state.lock().await;
            let cancelled = state.cancel.is_cancelled();
            (state.pending.take(), cancelled)
        };

        let Some(pending) = pending else {
            // A cancellation that raced ahead already resolved the batch —
            // the late approval keypress is benign, not an error.
            if !cancelled {
                let _ = tx.send(RuntimeEvent::Error {
                    turn_id: None,
                    message: "no pending tool approval".to_string(),
                });
            }
            return rx;
        };

        let runtime = self.clone();
        tokio::spawn(async move {
            emit(
                &tx,
                RuntimeEvent::ApprovalResolved {
                    turn_id: Some(pending.turn_id.clone()),
                    tool_call_id: ToolCallId::from(pending.current.id.clone()),
                    decision,
                },
            );
            runtime.handle_approval(pending, decision, &tx).await;
        });
        rx
    }

    /// Cancel the in-flight turn, if any. Idle runtimes treat this as a
    /// silent no-op.
    ///
    /// When the loop is streaming, it observes the token and finalizes on the
    /// turn's existing event channel; the returned receiver stays empty. When
    /// the turn is parked on an approval, no task is polling the token, so the
    /// cancellation is finalized here and its events (synthesized tool
    /// results, `TurnCancelled`) arrive on the returned receiver.
    pub async fn cancel_turn(&self) -> RuntimeEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let (token, pending, streaming) = {
            let mut state = self.state.lock().await;
            let token = state.cancel.clone();
            let pending = state.pending.take();
            let streaming = state.current_turn_id.is_some();
            (token, pending, streaming)
        };

        if let Some(pending) = pending {
            token.cancel();
            let runtime = self.clone();
            tokio::spawn(async move {
                runtime.finalize_cancelled_batch(pending, &tx).await;
            });
            return rx;
        }

        if streaming {
            token.cancel();
        }
        rx
    }

    /// Snapshot the current wire-derived message history. Mostly for
    /// tests/debugging.
    pub async fn session_messages(&self) -> Vec<Message> {
        self.state.lock().await.session.wire_messages()
    }

    async fn current_turn_id(&self) -> TurnId {
        self.state
            .lock()
            .await
            .current_turn_id
            .clone()
            .unwrap_or_else(TurnId::new)
    }

    async fn current_turn_context(&self) -> (TurnId, String) {
        let state = self.state.lock().await;
        (
            state.current_turn_id.clone().unwrap_or_else(TurnId::new),
            state.current_prompt.clone().unwrap_or_default(),
        )
    }

    async fn emit_session_updated(&self, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let state = self.state.lock().await;
        let message_count = state.session.wire_messages().len();
        let current_turn_id = state.current_turn_id.clone();
        drop(state);

        let (session_id, turn_count, summary, compaction, save_error, updated_at_ms) =
            if let Some(persistence) = self.persistence.as_ref() {
                let record = persistence.record.lock().await;
                (
                    Some(record.id.clone()),
                    record.turns.len(),
                    record.summary.clone(),
                    record.compaction.clone(),
                    persistence.actor.last_save_error(),
                    record.updated_at_ms,
                )
            } else {
                (None, 0, None, None, None, now_ms())
            };
        emit(
            tx,
            RuntimeEvent::SessionUpdated {
                session_id,
                current_turn_id,
                message_count,
                turn_count,
                summary,
                compaction,
                save_error,
                updated_at_ms,
            },
        );
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

#[cfg(test)]
#[path = "runtime/integration_tests.rs"]
mod tests;
