use crate::message::{Message, Role};
use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::subagent::roles::SubAgentRole;

/// The child's last substantive words, for a run that ends before the report
/// contract was reached (step cap, wall-clock timeout, cancel).
///
/// Finds the last NON-EMPTY assistant message, not merely the last assistant
/// message: at the step cap the final assistant turn is the empty content
/// carrier for the tool call that tripped the cap, so filtering emptiness only
/// *after* taking the last message returned `None` and threw away the findings
/// the child had written one or two turns earlier. This is the lookup
/// `headless::drive::final_assistant_text` already does correctly; shared so the
/// step-cap arm here and the wall-clock arm in `subagent::tools` agree.
pub(super) fn partial_report_from(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
        .map(|message| message.content.trim().to_string())
}

/// Drive a child runtime's turn loop to completion and return its final
/// report. Cancellation is *not* observed here via a side token — the caller
/// cancels the child through its own [`AgentRuntime::cancel_turn`], which the
/// loop surfaces as [`RuntimeEvent::TurnCancelled`]. On any non-success exit
/// the step count is still returned so the ledger records real progress.
///
/// `network` is the dispatch-time grant; the auto-decider needs it to tell a
/// "you have no network" denial from a plain allow-list denial, so the child
/// is pointed at the recovery that actually applies.
///
/// `on_progress` receives one short line per child tool call as it starts;
/// the parent's `agent` tool forwards these through `cx.update`, so a
/// minutes-long child shows live activity in the parent UI instead of a
/// frozen tool cell.
pub async fn run_subagent(
    runtime: AgentRuntime,
    max_steps: u32,
    role: SubAgentRole,
    network: bool,
    on_progress: impl Fn(String),
) -> Result<(String, u32), (u32, String, Option<String>)> {
    let started = std::time::Instant::now();
    let mut steps = 0u32;
    let mut budget_notice_sent = false;
    let mut rx = runtime.drive_turn().await;

    async fn partial_report(runtime: &AgentRuntime) -> Option<String> {
        partial_report_from(&runtime.session_messages().await)
    }

    loop {
        if steps >= max_steps {
            // Cancel before returning. The child's `run_loop` is a detached task
            // and `emit` ignores a closed channel, so simply returning left it
            // running: it kept calling the API and kept executing write tools
            // after the parent had already reported the failure and folded its
            // spend. The timeout arm in the `agent` tool cancels for exactly this
            // reason; this arm did not.
            let partial = partial_report(&runtime).await;
            let _ = runtime.cancel_turn().await;
            return Err((steps, format!("max steps exceeded ({max_steps})"), partial));
        }

        // Same soft end as the parent loop, in this loop's own unit (tool
        // calls): a child cut off at the cap returns a failure with whatever its
        // last message happened to be, and the only way it can hand over
        // properly is to be asked while it still has budget to answer.
        if crate::runtime::budget_wrap_up_due(steps, max_steps, budget_notice_sent) {
            budget_notice_sent = true;
            let text = crate::tr_with(
                runtime.ui_lang(),
                crate::TextId::SubagentBudgetWrapUp,
                &[
                    ("used", &steps.to_string()),
                    ("limit", &max_steps.to_string()),
                ],
            );
            // A runtime notice, not user input (see `steer_notice`): the child's
            // own turn loop records it as a System entry, so it cannot pull the
            // child's compaction anchor off its task the way a user entry would.
            let _ = runtime.steer_notice(text).await;
        }

        let Some(event) = rx.recv().await else {
            // Same reasoning: an ended stream does not stop the loop task.
            let partial = partial_report(&runtime).await;
            let _ = runtime.cancel_turn().await;
            return Err((
                steps,
                "sub-agent event stream ended unexpectedly".to_string(),
                partial,
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
                    return Err((
                        steps,
                        "sub-agent finished without a report".to_string(),
                        None,
                    ));
                }
                return Ok((text, steps));
            }
            RuntimeEvent::ApprovalRequired { request, .. } => {
                let (decision, denial_note) =
                    runtime.subagent_approval_decision(&request, role, network);
                rx = runtime
                    .submit_approval_with_denial_note(decision, denial_note)
                    .await;
            }
            RuntimeEvent::TurnCancelled { .. } => {
                return Err((steps, "cancelled".to_string(), None));
            }
            RuntimeEvent::Error { message, .. } => {
                let partial = partial_report(&runtime).await;
                return Err((steps, message, partial));
            }
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
            // RootGranted cannot fire in a child (root grants are auto-denied
            // for sub-agents), and UserMessageInjected cannot either: a child
            // turn is driven unattended, so nobody is there to steer it. Both
            // listed for exhaustiveness.
            RuntimeEvent::CheckpointCreated { .. }
            | RuntimeEvent::WorkspaceRestored { .. }
            | RuntimeEvent::DiagnosticsUpdated { .. }
            | RuntimeEvent::CompactionApplied { .. }
            | RuntimeEvent::RootGranted { .. }
            | RuntimeEvent::UserMessageInjected { .. }
            | RuntimeEvent::Warning { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::model::{ToolCallFunctionPayload, ToolCallPayload};

    /// At the step cap the child's final assistant turn is the empty content
    /// carrier for the tool call that tripped the cap, so the hand-off must look
    /// PAST it to the last turn that actually said something. Taking the last
    /// assistant message and only then filtering emptiness returned `None` and
    /// discarded the findings written a turn or two earlier.
    #[test]
    fn partial_report_skips_the_empty_tool_call_carrier() {
        let messages = vec![
            Message::user("find the bug"),
            Message::assistant_turn("Here is what I found: the race is in foo.rs.", "", vec![]),
            // The carrier for the final tool call: empty content, tool_calls set.
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCallPayload {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunctionPayload {
                        name: "shell".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
            ),
            Message::tool("call_1", "some output"),
        ];
        assert_eq!(
            partial_report_from(&messages).as_deref(),
            Some("Here is what I found: the race is in foo.rs."),
            "the last non-empty assistant turn is the hand-off, not the empty carrier"
        );
    }

    /// A whitespace-only final turn is also skipped, and a run that never
    /// produced assistant text yields `None` so the caller can tell "nothing to
    /// hand over" from an empty string.
    #[test]
    fn partial_report_is_none_when_nothing_was_said() {
        assert_eq!(partial_report_from(&[]), None);
        let only_user = vec![Message::user("x"), Message::tool("c", "out")];
        assert_eq!(partial_report_from(&only_user), None);
        let blank_tail = vec![
            Message::assistant_turn("real findings", "", vec![]),
            Message::assistant_turn("   \n  ", "", vec![]),
        ];
        assert_eq!(
            partial_report_from(&blank_tail).as_deref(),
            Some("real findings")
        );
    }
}
