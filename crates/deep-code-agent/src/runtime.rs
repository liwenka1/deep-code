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
use crate::execution_policy::SharedPermissionMode;
use crate::lsp::LspManager;
use crate::message::Message;
use crate::model_registry::ModelRegistry;
use crate::session::Session;
use crate::session_store::{TurnRecord, now_ms};
use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolRegistry};
use event::emit;
pub use event::{RuntimeEvent, RuntimeEventReceiver, ToolCallId, TurnId};
use state::{Persistence, RuntimeState};
pub use telemetry::{PrefixStatus, TurnTelemetry};

/// How long [`AgentRuntime::shutdown`] waits for a cancelled live turn to
/// finalize before proceeding anyway. Sized to cover the foreground-shell
/// cancel arm's process-group kill plus its bounded output drain (500ms);
/// nested sub-agent bookkeeping can lag past it, but its own group kill has
/// fired by then too, which is all shutdown needs.
const LIVE_TURN_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Poll interval for [`AgentRuntime::shutdown`]'s bounded wait above.
const LIVE_TURN_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Agent runtime tying together [`LlmClient`], [`ToolRegistry`], and [`Session`].
///
/// Cheap to clone: state is behind an [`Arc`]/[`Mutex`].
#[derive(Clone)]
pub struct AgentRuntime {
    client: Arc<dyn LlmClient>,
    config: AgentConfig,
    registry: ModelRegistry,
    is_subagent: bool,
    tools: Arc<ToolRegistry>,
    state: Arc<Mutex<RuntimeState>>,
    checkpoints: Option<Arc<CheckpointStore>>,
    workspace: Option<PathBuf>,
    lsp: Option<Arc<LspManager>>,
    persistence: Option<Arc<Persistence>>,
    /// Session permission mode, shared (lock-free) with the TUI so it can show
    /// the current mode and flip it on Shift+Tab. The approval gate reads it
    /// per gated call.
    permission_mode: SharedPermissionMode,
    /// UI language for runtime-rendered user-facing text. Shared (lock-free) so
    /// `/lang` can flip it live through the runtime handle without a relaunch
    /// (see [`Self::ui_lang`] / [`Self::set_ui_lang`]).
    ui_lang: crate::i18n::SharedLang,
}

impl AgentRuntime {
    pub fn new<C: LlmClient + 'static>(client: C, tools: ToolRegistry) -> Self {
        Self::with_config(client, tools, AgentConfig::default())
    }

    pub fn with_config<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        config: AgentConfig,
    ) -> Self {
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
            permission_mode: SharedPermissionMode::default(),
            ui_lang: crate::i18n::SharedLang::new(crate::i18n::Lang::from_env(&config.language)),
        }
    }

    pub fn with_system_prompt<C: LlmClient + 'static>(
        client: C,
        tools: ToolRegistry,
        system: impl Into<String>,
        config: AgentConfig,
        is_subagent: bool,
    ) -> Self {
        Self::with_system_prompt_shared(Arc::new(client), tools, system, config, is_subagent)
    }

    /// Like [`Self::with_system_prompt`] but from an already-shared client, so a
    /// child agent reuses the parent's `Arc<dyn LlmClient>` rather than cloning
    /// the concrete client.
    pub fn with_system_prompt_shared(
        client: Arc<dyn LlmClient>,
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
            client,
            config: config.clone(),
            registry: ModelRegistry::default(),
            is_subagent,
            tools: Arc::new(tools),
            state: Arc::new(Mutex::new(RuntimeState {
                session,
                ..Default::default()
            })),
            checkpoints: None,
            workspace: None,
            lsp: None,
            persistence: None,
            permission_mode: SharedPermissionMode::default(),
            ui_lang: crate::i18n::SharedLang::new(crate::i18n::Lang::from_env(&config.language)),
        }
    }

    /// Attach a shared permission-mode handle (created at launch and shared
    /// with the TUI). Builder-style so the constructors stay unchanged.
    #[must_use]
    pub fn with_permission_mode(mut self, mode: SharedPermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Attach the launch-created shared UI language (see [`Self::set_ui_lang`]).
    /// Builder-style, mirroring [`Self::with_permission_mode`]; lets the offline
    /// echo client and the runtime share one language atomic so `/lang` reaches
    /// both.
    #[must_use]
    pub fn with_ui_lang(mut self, ui_lang: crate::i18n::SharedLang) -> Self {
        self.ui_lang = ui_lang;
        self
    }

    /// The runtime's current session permission mode.
    #[must_use]
    pub fn permission_mode(&self) -> crate::execution_policy::PermissionMode {
        self.permission_mode.get()
    }

    /// Update the cached UI language live (the TUI calls this on `/lang` via the
    /// runtime handle). A lock-free store, so the next user-facing string the
    /// runtime renders picks it up without a relaunch.
    pub fn set_ui_lang(&self, lang: crate::i18n::Lang) {
        self.ui_lang.set(lang);
    }

    /// Shut down background resources: cancel any live turn (so its tools tear
    /// their child processes down, see [`Self::cancel_live_turn`]), then flush
    /// persistence and stop spawned LSP servers.
    pub async fn shutdown(&self) {
        self.cancel_live_turn().await;
        self.persist().await;
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.actor.flush().await;
        }
        if let Some(lsp) = self.lsp.as_ref() {
            lsp.shutdown_all().await;
        }
    }

    /// Cancel the in-flight turn, if any, and wait — bounded — for its loop to
    /// observe the token and finalize. The loop runs as its own task, so it is
    /// still polled while this waits; on cancel the foreground-shell arm kills
    /// the child's whole process group, which the `kill_on_drop` backstop at
    /// process exit cannot do (it signals only the group leader, so
    /// grandchildren — a foreground-run dev server, say — would survive and
    /// keep their port). Idle runtimes return on the first check. Best-effort:
    /// on grace expiry the shutdown proceeds anyway — the group kill fires on
    /// the tool's first poll after cancel, well before the bookkeeping that
    /// this waits on (turn finalization, sub-agent grace) can lag behind.
    async fn cancel_live_turn(&self) {
        drop(self.cancel_turn().await);
        let deadline = tokio::time::Instant::now() + LIVE_TURN_CANCEL_GRACE;
        while self.live_turn_id().await.is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(LIVE_TURN_CANCEL_POLL).await;
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
        // A previous turn may still be live (e.g. the HTTP client disconnected
        // mid-turn and a new prompt arrived): cancel its loop so it stops
        // streaming. The turn-id guard in `finish_turn` keeps that loop's late
        // finalization from consuming this turn's state.
        {
            let state = self.state.lock().await;
            if state.current_turn_id.is_some() {
                state.cancel.cancel();
            }
        }
        self.finalize_orphan_turn().await;
        let prompt = prompt.into();
        let turn_id = TurnId::new();
        {
            let mut state = self.state.lock().await;
            // Interrupted tool calls need no repair here: pending exchanges
            // synthesize their placeholder at wire derivation.
            state.session.push_user(&prompt);
            state.pending = None;
            state.current_turn = Some(TurnRecord::new());
            state.current_prompt = Some(prompt);
            state.current_turn_id = Some(turn_id);
            state.cancel = CancellationToken::new();
            // Per-turn struggle counter and the "triggered this turn" flag
            // reset; the `cascade_escalated` latch intentionally persists for
            // the rest of the session.
            state.turn_tool_errors = 0;
            state.cascade_triggered_this_turn = false;
            state.turn_cost = Default::default();
            state.turn_cache_hit_tokens = 0;
            state.turn_cache_miss_tokens = 0;
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
        self.cancel_turn_inner(None).await
    }

    /// Like [`cancel_turn`](Self::cancel_turn) but a no-op unless `turn_id` is
    /// still the current turn. The SSE lease drop passes the turn it was
    /// observing so a client disconnect cancels *that* turn — never a successor
    /// a new request already began on this shared runtime. (A normally-finished
    /// turn's lease still fires this on drop; the guard makes that harmless.)
    /// The check shares one lock scope with the decision, so no new turn can
    /// slip in between.
    pub async fn cancel_turn_if(&self, turn_id: TurnId) -> RuntimeEventReceiver {
        self.cancel_turn_inner(Some(turn_id)).await
    }

    async fn cancel_turn_inner(&self, only_if: Option<TurnId>) -> RuntimeEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let (token, pending, streaming) = {
            let mut state = self.state.lock().await;
            if let Some(want) = only_if.as_ref()
                && state.current_turn_id.as_ref() != Some(want)
            {
                return rx;
            }
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

    /// The id of the turn currently in flight, if any. The SSE server captures
    /// this after starting a turn so its lease can later cancel *that* turn on
    /// disconnect via [`cancel_turn_if`](Self::cancel_turn_if).
    pub async fn live_turn_id(&self) -> Option<TurnId> {
        self.state.lock().await.current_turn_id.clone()
    }

    /// This session's accumulated spend — cost plus the cache traffic behind
    /// it. A sub-agent runtime exposes it so the parent can fold the child's
    /// spend into its own session totals via `ToolCx::report_spend`.
    pub async fn session_spend(&self) -> crate::tool::ToolSpend {
        let state = self.state.lock().await;
        crate::tool::ToolSpend {
            cost: state.session_cost,
            cache_hit_tokens: state.session_cache_hit_tokens,
            cache_miss_tokens: state.session_cache_miss_tokens,
            cache_savings: state.session_cache_savings,
        }
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

    /// Resolve a pending tool call for an unattended sub-agent.
    ///
    /// Fail-closed against the execution policy, with one role-posture
    /// exception: dispatching a writing role (`implementer` — the only role
    /// whose `allows_writes` is true) *is* the write authorization, so
    /// workspace file writes are approved for it. That dispatch is in turn
    /// gated where writes prompt: the `agent` call itself requires approval
    /// for writing roles (see `ToolKind::SubAgent` in the policy engine), so
    /// this consent is granted by the human, not assumed. Shell and network
    /// still require the policy's own allow paths (trusted prefixes /
    /// auto_allow); hard denials are never overridden.
    pub fn subagent_approval_decision(
        &self,
        request: &ApprovalRequest,
        role: crate::subagent::SubAgentRole,
    ) -> ApprovalDecision {
        let call = ToolCall::new(
            request.call_id.clone(),
            request.tool_name.clone(),
            request.arguments.clone(),
        );
        let plan = self.tools.evaluate_tool(&call);
        if plan.denied_reason().is_some() {
            return ApprovalDecision::Denied;
        }
        if !plan.requires_approval {
            return ApprovalDecision::Approved;
        }
        let kind = crate::execution_policy::ExecPolicy::classify_tool(&request.tool_name);
        if role.allows_writes() && kind == crate::execution_policy::ToolKind::WriteFile {
            return ApprovalDecision::Approved;
        }
        ApprovalDecision::Denied
    }
}

#[cfg(test)]
#[path = "runtime/integration_tests.rs"]
mod tests;
