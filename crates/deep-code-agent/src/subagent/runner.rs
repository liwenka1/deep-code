use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::subagent::roles::SubAgentRole;

/// Drive a child runtime's turn loop to completion and return its final
/// report. Cancellation is *not* observed here via a side token — the caller
/// cancels the child through its own [`AgentRuntime::cancel_turn`], which the
/// loop surfaces as [`RuntimeEvent::TurnCancelled`]. On any non-success exit
/// the step count is still returned so the ledger records real progress.
///
/// `on_progress` receives one short line per child tool call as it starts;
/// the parent's `agent` tool forwards these through `cx.update`, so a
/// minutes-long child shows live activity in the parent UI instead of a
/// frozen tool cell.
pub async fn run_subagent(
    runtime: AgentRuntime,
    max_steps: u32,
    role: SubAgentRole,
    on_progress: impl Fn(String),
) -> Result<(String, u32), (u32, String)> {
    let started = std::time::Instant::now();
    let mut steps = 0u32;
    let mut rx = runtime.drive_turn().await;

    loop {
        if steps >= max_steps {
            // Cancel before returning. The child's `run_loop` is a detached task
            // and `emit` ignores a closed channel, so simply returning left it
            // running: it kept calling the API and kept executing write tools
            // after the parent had already reported the failure and folded its
            // spend. The timeout arm in the `agent` tool cancels for exactly this
            // reason; this arm did not.
            let _ = runtime.cancel_turn().await;
            return Err((steps, format!("max steps exceeded ({max_steps})")));
        }

        let Some(event) = rx.recv().await else {
            // Same reasoning: an ended stream does not stop the loop task.
            let _ = runtime.cancel_turn().await;
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
            RuntimeEvent::ToolCallStarted { tool_name, .. } => {
                // The upcoming step: `steps` counts finished calls. Each line
                // carries the role, elapsed wall clock and the step budget, so
                // the tail visible in the parent transcript reads as a pulse —
                // "is it stuck?" is answerable at a glance ("+41s step 7/50"),
                // which a bare "step 7" was not.
                on_progress(format!(
                    "[{}] +{}s step {}/{max_steps}: {tool_name}",
                    role.as_str(),
                    started.elapsed().as_secs(),
                    steps + 1,
                ));
            }
            RuntimeEvent::ToolCallFinished { .. } => {
                steps += 1;
            }
            RuntimeEvent::TurnStarted { .. }
            | RuntimeEvent::AssistantDelta { .. }
            | RuntimeEvent::ReasoningDelta { .. }
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
