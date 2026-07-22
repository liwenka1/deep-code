use tokio_util::sync::CancellationToken;

use crate::client::LlmClient;
use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::subagent::roles::SubAgentRole;

/// Drive a child runtime's turn loop to completion: consume events, resolve
/// approvals by role posture, count tool steps, and return the final
/// assistant message.
pub async fn run_subagent<C: LlmClient + Clone + 'static>(
    runtime: AgentRuntime<C>,
    cancel: CancellationToken,
    max_steps: u32,
    role: SubAgentRole,
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
            RuntimeEvent::ApprovalRequired { request, .. } => {
                let decision = runtime.subagent_approval_decision(&request, role);
                rx = runtime.submit_approval(decision).await;
            }
            RuntimeEvent::TurnCancelled { .. } => return Err("cancelled".to_string()),
            RuntimeEvent::Error { message, .. } => return Err(message),
            RuntimeEvent::ToolCallFinished { .. } => {
                steps += 1;
            }
            RuntimeEvent::TurnStarted { .. }
            | RuntimeEvent::AssistantDelta { .. }
            | RuntimeEvent::ReasoningDelta { .. }
            | RuntimeEvent::ToolCallStarted { .. }
            | RuntimeEvent::ToolCallUpdated { .. }
            | RuntimeEvent::ToolCallProgress { .. }
            | RuntimeEvent::ApprovalResolved { .. }
            | RuntimeEvent::SessionUpdated { .. } => {}
            RuntimeEvent::CheckpointCreated { .. }
            | RuntimeEvent::WorkspaceRestored { .. }
            | RuntimeEvent::DiagnosticsUpdated { .. }
            | RuntimeEvent::CompactionApplied { .. }
            | RuntimeEvent::Warning { .. } => {}
        }
    }
}
