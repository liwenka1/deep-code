use crate::client::LlmClient;
use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::subagent::roles::SubAgentRole;

/// Drive a child runtime's turn loop to completion and return its final
/// report. Cancellation is *not* observed here via a side token — the caller
/// cancels the child through its own [`AgentRuntime::cancel_turn`], which the
/// loop surfaces as [`RuntimeEvent::TurnCancelled`]. On any non-success exit
/// the step count is still returned so the ledger records real progress.
pub async fn run_subagent<C: LlmClient + Clone + 'static>(
    runtime: AgentRuntime<C>,
    max_steps: u32,
    role: SubAgentRole,
) -> Result<(String, u32), (u32, String)> {
    let mut steps = 0u32;
    let mut rx = runtime.drive_turn().await;

    loop {
        if steps >= max_steps {
            return Err((steps, format!("max steps exceeded ({max_steps})")));
        }

        let Some(event) = rx.recv().await else {
            return Err((
                steps,
                "sub-agent event stream ended unexpectedly".to_string(),
            ));
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
                if text.trim().is_empty() {
                    // The turn ended without an assistant report (e.g. the model
                    // stopped on a tool call). Surface it as a failure rather
                    // than handing the parent an empty "success".
                    return Err((steps, "sub-agent finished without a report".to_string()));
                }
                return Ok((text, steps));
            }
            RuntimeEvent::ApprovalRequired { request, .. } => {
                let decision = runtime.subagent_approval_decision(&request, role);
                rx = runtime.submit_approval(decision).await;
            }
            RuntimeEvent::TurnCancelled { .. } => return Err((steps, "cancelled".to_string())),
            RuntimeEvent::Error { message, .. } => return Err((steps, message)),
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
