use tokio::sync::mpsc;

use super::{AgentRuntime, RuntimeEvent, RuntimeEventReceiver, TurnId, TurnTelemetry, UserTurn};
use crate::Usage;
use crate::tool::{ApprovalDecision, ApprovalError, ApprovalId};

/// The runtime's authoritative reason for ending a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Completed,
    Cancelled,
    StepLimit,
    Failed,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub reason: FinishReason,
    pub usage: Option<Usage>,
    pub telemetry: Option<TurnTelemetry>,
    pub error: Option<String>,
}

/// One event stream for an entire turn, including every approval pause.
/// Dropping an unfinished handle cancels only the turn it owns.
pub struct TurnHandle {
    runtime: AgentRuntime,
    id: TurnId,
    events: RuntimeEventReceiver,
    outcome: Option<TurnOutcome>,
    cancel: tokio_util::sync::CancellationToken,
}

impl TurnHandle {
    pub fn id(&self) -> &TurnId {
        &self.id
    }

    pub fn outcome(&self) -> Option<&TurnOutcome> {
        self.outcome.as_ref()
    }

    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if self.outcome.is_some() {
            return None;
        }
        let event = self.events.recv().await;
        let terminal = match &event {
            Some(RuntimeEvent::TurnFinished {
                usage, telemetry, ..
            }) => Some(TurnOutcome {
                reason: FinishReason::Completed,
                usage: usage.clone(),
                telemetry: telemetry.clone(),
                error: None,
            }),
            Some(RuntimeEvent::TurnCancelled { .. }) => Some(TurnOutcome {
                reason: FinishReason::Cancelled,
                usage: None,
                telemetry: None,
                error: Some("turn cancelled".into()),
            }),
            Some(RuntimeEvent::Error { kind, message, .. }) => Some(TurnOutcome {
                reason: match kind {
                    super::RuntimeFailureKind::StepLimit => FinishReason::StepLimit,
                    super::RuntimeFailureKind::Execution => FinishReason::Failed,
                },
                usage: None,
                telemetry: None,
                error: Some(message.clone()),
            }),
            None => Some(TurnOutcome {
                reason: FinishReason::Incomplete,
                usage: None,
                telemetry: None,
                error: Some("turn event stream ended without a terminal event".into()),
            }),
            _ => None,
        };
        self.outcome = terminal;
        event
    }

    pub async fn approve(
        &self,
        approval_id: &ApprovalId,
        decision: ApprovalDecision,
        denial_note: Option<String>,
    ) -> Result<(), ApprovalError> {
        self.runtime
            .submit_approval_with_denial_note(approval_id, decision, denial_note)
            .await
            .map(drop)
    }

    pub async fn cancel(&self) {
        self.cancel.cancel();
        drop(self.runtime.cancel_turn_if(self.id.clone()).await);
    }
}

impl Drop for TurnHandle {
    fn drop(&mut self) {
        if self
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.reason != FinishReason::Incomplete)
        {
            return;
        }
        self.cancel.cancel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = self.runtime.clone();
            let id = self.id.clone();
            handle.spawn(async move {
                drop(runtime.cancel_turn_if(id).await);
            });
        }
    }
}

impl AgentRuntime {
    /// A panicking provider/tool must not leave the stable sender in state
    /// forever. Approval pauses are normal returns and retain that sender.
    pub(super) async fn supervise_turn(
        &self,
        turn_id: &TurnId,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        work: impl std::future::Future<Output = ()>,
    ) {
        use futures_util::FutureExt;
        if std::panic::AssertUnwindSafe(work)
            .catch_unwind()
            .await
            .is_err()
        {
            super::event::emit(
                tx,
                RuntimeEvent::Error {
                    kind: super::RuntimeFailureKind::Execution,
                    turn_id: Some(turn_id.clone()),
                    message: "turn task panicked".into(),
                },
            );
            self.abort_turn(turn_id).await;
        }
    }

    pub async fn start_turn(&self, prompt: impl Into<UserTurn>) -> TurnHandle {
        // The bootstrap owns begin_turn even when its caller stops awaiting
        // during persistence. Sending to a dropped receiver drops the newly
        // created handle and cancels that exact turn before any tools run.
        let (ready, receive) = tokio::sync::oneshot::channel();
        let runtime = self.clone();
        let prompt = prompt.into();
        tokio::spawn(async move {
            let id = runtime.begin_turn(prompt).await;
            let turn = runtime.drive_turn_handle(id).await;
            let _ = ready.send(turn);
        });
        receive.await.expect("runtime turn bootstrap stopped")
    }

    /// Drive a turn prepared by `begin_turn` (used by the sub-agent launcher).
    pub(crate) async fn drive_turn_handle(&self, id: TurnId) -> TurnHandle {
        let (tx, events) = mpsc::unbounded_channel();
        let (prompt, cancel) = {
            let mut state = self.state.lock().await;
            if state.current_turn_id.as_ref() != Some(&id) {
                let cancel = tokio_util::sync::CancellationToken::new();
                cancel.cancel();
                super::event::emit(
                    &tx,
                    RuntimeEvent::TurnCancelled {
                        turn_id: id.clone(),
                    },
                );
                return TurnHandle {
                    runtime: self.clone(),
                    id,
                    events,
                    outcome: None,
                    cancel,
                };
            }
            state.turn_events = Some((id.clone(), tx.clone()));
            (
                state.current_prompt.clone().unwrap_or_default().text,
                state.cancel.clone(),
            )
        };
        let turn = TurnHandle {
            runtime: self.clone(),
            id: id.clone(),
            events,
            outcome: None,
            cancel: cancel.clone(),
        };
        // Checkpointing and the model loop share one supervised task, so
        // dropping a handle while a snapshot runs still has a cancellation owner.
        self.spawn_turn(id, prompt, cancel, tx);
        turn
    }

    /// Low-level step APIs use a private channel unless a TurnHandle owns the
    /// turn. With a handle every resume/cancel emits on its original channel.
    pub(super) async fn event_channel(
        &self,
        turn_id: Option<&TurnId>,
    ) -> (mpsc::UnboundedSender<RuntimeEvent>, RuntimeEventReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = self.state.lock().await;
        if let Some((owner, stable)) = state.turn_events.as_ref()
            && Some(owner) == turn_id
        {
            return (stable.clone(), rx);
        }
        (tx, rx)
    }
}
