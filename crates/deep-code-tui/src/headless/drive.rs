//! Event-loop driver for one unattended turn.
//!
//! Uses the runtime TurnHandle with an unattended approval policy: an
//! approval that would prompt is denied deterministically and the turn
//! continues. A headless run must never park waiting for a decision nobody
//! will send. Capability is granted the same way as everywhere else —
//! permission mode, `approval.auto_allow` — never by a headless-only bypass.

use deep_code_agent::{
    AgentRuntime, ApprovalDecision, RuntimeEvent, TurnTelemetry, Usage, unattended_denial_note,
};

/// How the driven turn ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DriveStatus {
    Finished,
    Cancelled,
    Failed(String),
    /// The event channel closed without a terminal event. Happens when a
    /// runtime task exits unexpectedly.
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
    on_event: &mut impl AsyncFnMut(&RuntimeEvent) -> std::io::Result<()>,
) -> DriveOutcome {
    let mut outcome = DriveOutcome {
        status: DriveStatus::Incomplete,
        reasoning: String::new(),
        usage: None,
        telemetry: None,
        denied_approvals: 0,
    };

    let mut events = runtime.start_turn(prompt).await;
    while let Some(event) = events.recv().await {
        if let Err(error) = on_event(&event).await {
            events.cancel().await;
            outcome.status = DriveStatus::Failed(format!("output failed: {error}"));
            return outcome;
        }
        match event {
            RuntimeEvent::ReasoningDelta { text, .. } => outcome.reasoning.push_str(&text),
            RuntimeEvent::ApprovalRequired { ref request, .. } => {
                outcome.denied_approvals += 1;
                if let Err(error) = events
                    .approve(
                        &request.approval_id,
                        ApprovalDecision::Denied,
                        Some(unattended_denial_note(request)),
                    )
                    .await
                {
                    outcome.status = DriveStatus::Failed(error.to_string());
                    return outcome;
                }
            }
            _ => {}
        }
    }
    if let Some(result) = events.outcome() {
        use deep_code_agent::FinishReason;
        outcome.usage = result.usage.clone();
        outcome.telemetry = result.telemetry.clone();
        outcome.status = match result.reason {
            FinishReason::Completed => DriveStatus::Finished,
            FinishReason::Cancelled => DriveStatus::Cancelled,
            FinishReason::Incomplete => DriveStatus::Incomplete,
            FinishReason::StepLimit | FinishReason::Failed => {
                DriveStatus::Failed(result.error.clone().unwrap_or_default())
            }
        };
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::{
        AgentEvent, AgentResult, ChatRequest, FunctionCallDelta, Message, MockEchoTool, Role,
        ToolCallDelta, ToolRegistry,
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

        let outcome = drive_to_completion(&runtime, "hi".to_string(), &mut async |_| Ok(())).await;

        assert_eq!(outcome.status, DriveStatus::Finished);
        assert_eq!(outcome.reasoning, "thinking…");
        assert_eq!(outcome.denied_approvals, 0);
        let messages = runtime.session_messages().await;
        assert_eq!(
            deep_code_agent::last_assistant_text(&messages).as_deref(),
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
        let outcome = drive_to_completion(&runtime, "hi".to_string(), &mut async |event| {
            if matches!(event, RuntimeEvent::ApprovalRequired { .. }) {
                saw_approval_event = true;
            }
            Ok(())
        })
        .await;

        assert!(
            saw_approval_event,
            "the denial must be observable, not silent"
        );
        assert_eq!(outcome.denied_approvals, 1);
        assert_eq!(outcome.status, DriveStatus::Finished);
        let messages = runtime.session_messages().await;
        assert_eq!(
            deep_code_agent::last_assistant_text(&messages).as_deref(),
            Some("done")
        );
    }

    /// The auto-denial the model reads must not claim a human refused it.
    ///
    /// `-p` denies every prompt because nobody can answer one, and it used to
    /// submit a bare `Denied` — so the tool result the model saw was the
    /// registry's stock "Tool call denied by user." A model that believes a
    /// person said no plans around a decision, instead of reporting what it
    /// needed so the operator can re-run with it granted. Same false fact the
    /// sub-agent gate carries its own notes to avoid; this is the consumer
    /// `runtime`'s module doc already named as adding an auto-deny on top.
    ///
    /// Asserted on the recorded transcript, not on the count: `denied_approvals`
    /// was already right while the text was wrong.
    #[tokio::test]
    async fn the_auto_denial_tells_the_model_no_user_was_asked() {
        let runtime = AgentRuntime::new(
            ScriptedClient::new(vec![tool_call_events(), text_events("done", None)]),
            ToolRegistry::with_mock_tools(),
        );

        let outcome = drive_to_completion(&runtime, "hi".to_string(), &mut async |_| Ok(())).await;
        assert_eq!(outcome.denied_approvals, 1);

        let messages = runtime.session_messages().await;
        let denied = messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .expect("the denied call keeps its paired tool message");
        assert!(
            denied.content.text().contains("no user saw this request"),
            "the model must be told the run is unattended: {}",
            denied.content.summary_text()
        );
        assert!(
            !denied.content.text().contains("denied by user"),
            "the stock human-refusal text must not reach the model: {}",
            denied.content.summary_text()
        );
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
            deep_code_agent::last_assistant_text(&messages).as_deref(),
            Some("the answer")
        );
        assert_eq!(
            deep_code_agent::last_assistant_text(&[Message::user("x")]),
            None
        );
    }
}
