use serde::{Deserialize, Serialize};

use crate::model::{StreamChunk, ToolCallDelta, Usage};

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

        if choice.finish_reason.is_some() {
            events.push(AgentEvent::Done {
                usage: chunk.usage.clone(),
            });
        }
    }

    if events.is_empty() && chunk.usage.is_some() {
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
            id: Some("chunk".to_string()),
            model: Some("deepseek-v4-pro".to_string()),
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
}
