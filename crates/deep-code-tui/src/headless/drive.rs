//! Event-loop driver for one unattended turn.
//!
//! Mirrors the HTTP server's SSE loop (`deep-code-runtime/src/server.rs`,
//! `prompt_sse`) with the `--approval-mode autonomous` posture baked in: an
//! approval that would prompt is denied deterministically and the turn
//! continues. A headless run must never park waiting for a decision nobody
//! will send. Capability is granted the same way as everywhere else —
//! permission mode, `approval.auto_allow` — never by a headless-only bypass.

use deep_code_agent::{
    AgentRuntime, ApprovalDecision, Message, Role, RuntimeEvent, TurnTelemetry, Usage,
};

/// How the driven turn ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DriveStatus {
    Finished,
    Cancelled,
    Failed(String),
    /// The event channel closed without a terminal event. Happens when a
    /// cancel races the approval hand-off and finalizes on a channel this
    /// loop never held; the caller decides what that means (interrupt,
    /// timeout, or a genuine defect).
    Incomplete,
}

#[derive(Debug)]
pub(crate) struct DriveOutcome {
    pub status: DriveStatus,
    /// Whole-turn reasoning text, accumulated across every request of the
    /// turn — same shape the bot posts into its folded "thinking" block.
    pub reasoning: String,
    pub usage: Option<Usage>,
    pub telemetry: Option<TurnTelemetry>,
    /// Gated calls that were auto-denied. Non-zero explains "why didn't it
    /// do X" to the caller, so it is surfaced rather than swallowed.
    pub denied_approvals: u32,
}

/// Drive one turn to a terminal state. Every event is offered to `on_event`
/// (for NDJSON mirroring / verbose trace) before this loop interprets it.
pub(crate) async fn drive_to_completion(
    runtime: &AgentRuntime,
    prompt: String,
    on_event: &mut dyn FnMut(&RuntimeEvent),
) -> DriveOutcome {
    let mut outcome = DriveOutcome {
        status: DriveStatus::Incomplete,
        reasoning: String::new(),
        usage: None,
        telemetry: None,
        denied_approvals: 0,
    };

    let mut events = runtime.submit_user(prompt).await;
    'turn: loop {
        let mut resumed = false;
        while let Some(event) = events.recv().await {
            on_event(&event);
            match event {
                RuntimeEvent::ReasoningDelta { text, .. } => outcome.reasoning.push_str(&text),
                RuntimeEvent::ApprovalRequired { .. } => {
                    // Deny, never park (see module docs). `submit_approval`
                    // resumes the batch on a fresh channel.
                    outcome.denied_approvals += 1;
                    events = runtime.submit_approval(ApprovalDecision::Denied).await;
                    resumed = true;
                    break;
                }
                RuntimeEvent::TurnFinished {
                    usage, telemetry, ..
                } => {
                    outcome.usage = usage;
                    outcome.telemetry = telemetry;
                    outcome.status = DriveStatus::Finished;
                    break 'turn;
                }
                RuntimeEvent::TurnCancelled { .. } => {
                    outcome.status = DriveStatus::Cancelled;
                    break 'turn;
                }
                RuntimeEvent::Error { message, .. } => {
                    outcome.status = DriveStatus::Failed(message);
                    break 'turn;
                }
                _ => {}
            }
        }
        if !resumed {
            break;
        }
    }
    outcome
}

/// The turn's answer: the last assistant message with visible content.
/// Reading the session (not concatenating deltas) keeps narration the model
/// emitted between tool calls out of the printed result.
pub(crate) fn final_assistant_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
        .map(|message| message.content.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::{
        AgentEvent, AgentResult, ChatRequest, FunctionCallDelta, MockEchoTool, ToolCallDelta,
        ToolRegistry,
    };
    use std::sync::Mutex as StdMutex;

    /// Scripted model: emits the given event scripts, one per request.
    /// The last script repeats if the runtime asks for more turns.
    struct ScriptedClient {
        scripts: Vec<Vec<AgentEvent>>,
        requests: StdMutex<usize>,
    }

    impl ScriptedClient {
        fn new(scripts: Vec<Vec<AgentEvent>>) -> Self {
            Self {
                scripts,
                requests: StdMutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl deep_code_agent::LlmClient for ScriptedClient {
        fn provider_name(&self) -> &'static str {
            "scripted"
        }

        fn model(&self) -> &str {
            "scripted"
        }

        async fn stream_chat(
            &self,
            _request: ChatRequest,
        ) -> AgentResult<deep_code_agent::AgentEventStream> {
            let index = {
                let mut requests = self.requests.lock().unwrap();
                let current = *requests;
                *requests += 1;
                current.min(self.scripts.len().saturating_sub(1))
            };
            let script = self.scripts[index].clone();
            let stream = async_stream::try_stream! {
                for event in script {
                    yield event;
                }
            };
            Ok(Box::pin(stream))
        }
    }

    fn text_events(text: &str, reasoning: Option<&str>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if let Some(reasoning) = reasoning {
            events.push(AgentEvent::ReasoningDelta {
                text: reasoning.to_string(),
            });
        }
        events.push(AgentEvent::TextDelta {
            text: text.to_string(),
        });
        events.push(AgentEvent::Done { usage: None });
        events
    }

    fn tool_call_events() -> Vec<AgentEvent> {
        vec![
            AgentEvent::ToolCallDelta {
                delta: ToolCallDelta {
                    index: Some(0),
                    id: Some("call_1".to_string()),
                    call_type: Some("function".to_string()),
                    function: Some(FunctionCallDelta {
                        name: Some(MockEchoTool::NAME.to_string()),
                        arguments: Some(r#"{"message":"hello"}"#.to_string()),
                    }),
                },
            },
            AgentEvent::Done { usage: None },
        ]
    }

    #[tokio::test]
    async fn plain_text_turn_finishes_and_separates_answer_from_reasoning() {
        let runtime = AgentRuntime::new(
            ScriptedClient::new(vec![text_events("final answer", Some("thinking…"))]),
            ToolRegistry::with_mock_tools(),
        );

        let outcome = drive_to_completion(&runtime, "hi".to_string(), &mut |_| {}).await;

        assert_eq!(outcome.status, DriveStatus::Finished);
        assert_eq!(outcome.reasoning, "thinking…");
        assert_eq!(outcome.denied_approvals, 0);
        let messages = runtime.session_messages().await;
        assert_eq!(
            final_assistant_text(&messages).as_deref(),
            Some("final answer")
        );
    }

    #[tokio::test]
    async fn gated_tool_is_auto_denied_and_the_turn_still_completes() {
        // Request 0 asks for a gated mock tool; the resumed request answers
        // with text. In interactive mode this parks on a human — here it must
        // deny and keep going (the whole point of the headless posture).
        let runtime = AgentRuntime::new(
            ScriptedClient::new(vec![tool_call_events(), text_events("done", None)]),
            ToolRegistry::with_mock_tools(),
        );

        let mut saw_approval_event = false;
        let outcome = drive_to_completion(&runtime, "hi".to_string(), &mut |event| {
            if matches!(event, RuntimeEvent::ApprovalRequired { .. }) {
                saw_approval_event = true;
            }
        })
        .await;

        assert!(
            saw_approval_event,
            "the denial must be observable, not silent"
        );
        assert_eq!(outcome.denied_approvals, 1);
        assert_eq!(outcome.status, DriveStatus::Finished);
        let messages = runtime.session_messages().await;
        assert_eq!(final_assistant_text(&messages).as_deref(), Some("done"));
    }

    #[test]
    fn final_text_skips_tool_call_shells_and_narration() {
        let messages = vec![
            Message::user("do it"),
            // Narration before a tool call: visible content, but not the answer.
            Message::assistant_turn("let me look", "", Vec::new()),
            // Tool-call carrier with empty content must be skipped.
            Message::assistant_with_tool_calls("", Vec::new()),
            Message::assistant_turn("the answer\n", "", Vec::new()),
        ];
        assert_eq!(
            final_assistant_text(&messages).as_deref(),
            Some("the answer")
        );
        assert_eq!(final_assistant_text(&[Message::user("x")]), None);
    }
}
