use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::message::{Role, last_assistant_text};
use crate::runtime::{AgentRuntime, RuntimeEvent};
use crate::subagent::roles::SubAgentRole;

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
/// `steps_seen` mirrors the finished-call count as it grows, for a caller that
/// stops waiting before this returns: the wall-clock arm cancels the child and
/// gives it a grace window to unwind, and when the window elapses it has no
/// `(steps, …)` from here — without the mirror it recorded `0` steps for a
/// child that may have worked for an hour.
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
    steps_seen: Arc<AtomicU32>,
    on_progress: impl Fn(String),
) -> Result<(String, u32), (u32, String, Option<String>)> {
    let started = std::time::Instant::now();
    let mut steps = 0u32;
    let mut budget_notice_sent = false;
    let mut rx = runtime.drive_turn().await;

    /// The child's last substantive words, for a run that ends before the
    /// report contract was reached (step cap, a dead stream, an error).
    async fn partial_report(runtime: &AgentRuntime) -> Option<String> {
        last_assistant_text(&runtime.session_messages().await)
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
                // The report contract is the turn's FINAL assistant message.
                // An empty one (a reasoning-only reply, a stop right after a
                // tool call) is no report and fails rather than handing the
                // parent an empty "success" — but what the child said before it
                // still goes back as the partial hand-off, as on every other
                // failure arm. Returning `None` here threw those findings away.
                let report = messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::Assistant)
                    .filter(|message| !message.content.trim().is_empty());
                return match report {
                    Some(report) => Ok((report.content.clone(), steps)),
                    None => Err((
                        steps,
                        "sub-agent finished without a report".to_string(),
                        last_assistant_text(&messages),
                    )),
                };
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
                steps_seen.store(steps, Ordering::Relaxed);
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
    use crate::client::{AgentEventStream, LlmClient};
    use crate::error::AgentResult;
    use crate::event::AgentEvent;
    use crate::message::Message;
    use crate::model::{
        ChatRequest, FunctionCallDelta, ToolCallDelta, ToolCallFunctionPayload, ToolCallPayload,
    };
    use crate::tool::{MockEchoTool, ToolRegistry};

    /// Request 1 states findings and calls a tool; every later request answers
    /// with nothing at all — the empty final message a reasoning-only reply or a
    /// stop right after a tool call leaves behind.
    #[derive(Default)]
    struct FindingsThenSilence {
        requests: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl LlmClient for FindingsThenSilence {
        fn provider_name(&self) -> &'static str {
            "findings-then-silence"
        }

        fn model(&self) -> &str {
            "findings-then-silence"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let first = {
                let mut requests = self.requests.lock().unwrap();
                *requests += 1;
                *requests == 1
            };
            let mut events = Vec::new();
            if first {
                events.push(AgentEvent::TextDelta {
                    text: "Found it: the race is in foo.rs:42.".to_string(),
                });
                events.push(AgentEvent::ToolCallDelta {
                    delta: ToolCallDelta {
                        index: Some(0),
                        id: Some("call_1".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(FunctionCallDelta {
                            name: Some(MockEchoTool::NAME.to_string()),
                            arguments: Some(r#"{"message":"check"}"#.to_string()),
                        }),
                    },
                });
            }
            events.push(AgentEvent::Done { usage: None });
            Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            )))
        }
    }

    /// A turn that FINISHES on an empty assistant message is no report, but the
    /// findings the child wrote before it are the partial hand-off — the
    /// finished arm returned `None` there, the very bug the other failure arms
    /// had already been fixed for. The step mirror the wall-clock arm reads
    /// tracks the count this returns.
    #[tokio::test]
    async fn a_turn_ending_on_an_empty_message_hands_back_what_came_before() {
        let runtime = AgentRuntime::new(
            FindingsThenSilence::default(),
            ToolRegistry::with_mock_tools(),
        );
        runtime.begin_turn("find the race").await;
        let steps_seen = Arc::new(AtomicU32::new(0));
        let outcome = run_subagent(
            runtime,
            10,
            SubAgentRole::Explore,
            false,
            Arc::clone(&steps_seen),
            |_| {},
        )
        .await;
        let Err((steps, reason, partial)) = outcome else {
            panic!("an empty final message is no report: {outcome:?}");
        };
        assert_eq!(reason, "sub-agent finished without a report");
        assert_eq!(
            partial.as_deref(),
            Some("Found it: the race is in foo.rs:42.")
        );
        assert_eq!(steps, 1);
        assert_eq!(steps_seen.load(Ordering::Relaxed), steps);
    }

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
            last_assistant_text(&messages).as_deref(),
            Some("Here is what I found: the race is in foo.rs."),
            "the last non-empty assistant turn is the hand-off, not the empty carrier"
        );
    }

    /// A whitespace-only final turn is also skipped, and a run that never
    /// produced assistant text yields `None` so the caller can tell "nothing to
    /// hand over" from an empty string.
    #[test]
    fn partial_report_is_none_when_nothing_was_said() {
        assert_eq!(last_assistant_text(&[]), None);
        let only_user = vec![Message::user("x"), Message::tool("c", "out")];
        assert_eq!(last_assistant_text(&only_user), None);
        let blank_tail = vec![
            Message::assistant_turn("real findings", "", vec![]),
            Message::assistant_turn("   \n  ", "", vec![]),
        ];
        assert_eq!(
            last_assistant_text(&blank_tail).as_deref(),
            Some("real findings")
        );
    }
}
