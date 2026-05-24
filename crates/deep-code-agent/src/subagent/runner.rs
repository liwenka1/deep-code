use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::tool::ApprovalDecision;

use super::types::DEFAULT_MAX_STEPS;

pub async fn run_subagent<C: LlmClient + Clone + 'static>(
    _client: std::sync::Arc<C>,
    runtime: AgentRuntime<C>,
    cancel: CancellationToken,
    max_steps: u32,
) -> Result<(String, u32), String> {
    let mut steps = 0u32;
    let mut rx = runtime.drive_turn().await;

    loop {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        if steps >= max_steps {
            return Err(format!("max steps exceeded ({max_steps})"));
        }

        let Some(event) = rx.recv().await else {
            return Err("sub-agent event stream ended unexpectedly".to_string());
        };

        match event {
            RuntimeEvent::TurnFinished { .. } => {
                let messages = runtime.session_messages().await;
                let text = messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, crate::message::Role::Assistant))
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                return Ok((text, steps));
            }
            RuntimeEvent::ApprovalRequired { request } => {
                let decision = runtime.subagent_approval_decision(&request);
                if decision == ApprovalDecision::Approved {
                    steps += 1;
                }
                rx = runtime.submit_approval(decision).await;
            }
            RuntimeEvent::Error { message } => return Err(message),
            RuntimeEvent::ToolResult { .. } => {
                steps += 1;
            }
            RuntimeEvent::Provider(_) => {}
            RuntimeEvent::CheckpointCreated { .. }
            | RuntimeEvent::WorkspaceRestored { .. }
            | RuntimeEvent::DiagnosticsUpdated { .. }
            | RuntimeEvent::CompactionApplied { .. } => {}
        }
    }
}

#[must_use]
pub fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}
