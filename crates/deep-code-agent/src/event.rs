use serde::{Deserialize, Serialize};

use crate::model::{StreamChunk, ToolCallDelta, Usage};

/// Provider-stream events. These are produced by an [`crate::LlmClient`] and
/// represent only what comes back from the model API. Approval requests and
/// tool results are *not* provider events; they are synthesized by the agent
/// runtime — see [`crate::runtime::RuntimeEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallDelta { delta: ToolCallDelta },
    Done { usage: Option<Usage> },
    Error { message: String },
}

#[must_use]
pub fn chunk_to_events(chunk: StreamChunk) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut saw_finish = false;

    for choice in chunk.choices {
        if let Some(delta) = choice.delta {
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                events.push(AgentEvent::TextDelta { text });
            }

            if let Some(text) = delta.reasoning_content.filter(|text| !text.is_empty()) {
                events.push(AgentEvent::ReasoningDelta { text });
            }

            if let Some(tool_calls) = delta.tool_calls {
                events.extend(
                    tool_calls
                        .into_iter()
                        .map(|delta| AgentEvent::ToolCallDelta { delta }),
                );
            }
        }

        saw_finish |= choice.finish_reason.is_some();
    }

    // `usage` is request-level, so a request emits at most one `Done` carrying
    // it. Collapsing across choices matters when a provider streams n>1 choices
    // that each report a finish_reason — one `Done` per choice would make the
    // turn loop count the request's usage several times.
    if saw_finish {
        events.push(AgentEvent::Done {
            usage: chunk.usage.clone(),
        });
    } else if events.is_empty() && chunk.usage.is_some() {
        // A trailing usage-only chunk (no finish_reason, no content) still
        // carries the request's final usage.
        events.push(AgentEvent::Done { usage: chunk.usage });
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatChoice, ChoiceDelta};

    #[test]
    fn chunk_to_events_emits_text_and_done() {
        let chunk = StreamChunk {
            choices: vec![ChatChoice {
                index: 0,
                message: None,
                delta: Some(ChoiceDelta {
                    role: None,
                    content: Some("hello".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                }),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                total_tokens: Some(3),
                ..Usage::default()
            }),
        };

        assert_eq!(
            chunk_to_events(chunk),
            vec![
                AgentEvent::TextDelta {
                    text: "hello".to_string()
                },
                AgentEvent::Done {
                    usage: Some(Usage {
                        total_tokens: Some(3),
                        ..Usage::default()
                    })
                }
            ]
        );
    }

    #[test]
    fn multiple_finished_choices_emit_a_single_done() {
        // A provider streaming n>1 choices that each report a finish_reason must
        // still yield ONE Done — usage is request-level, so a Done per choice
        // would make the turn loop count the request's cost several times.
        let finished = |index| ChatChoice {
            index,
            message: None,
            delta: None,
            finish_reason: Some("stop".to_string()),
        };
        let chunk = StreamChunk {
            choices: vec![finished(0), finished(1)],
            usage: Some(Usage {
                total_tokens: Some(5),
                ..Usage::default()
            }),
        };

        let dones = chunk_to_events(chunk)
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::Done { .. }))
            .count();
        assert_eq!(dones, 1, "two finished choices must collapse to one Done");
    }
}
