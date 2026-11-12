//! Reassembly of streamed tool-call deltas into complete [`ToolCall`]s.

use std::collections::HashMap;

use serde_json::Value;

use crate::model::{FunctionCallDelta, ToolCallDelta};

use super::{ToolCall, ToolError};

/// Maximum JSON argument bytes accepted for one streamed tool call.
///
/// Tool arguments are model-controlled input. Keeping this below the overall
/// response cap prevents one call from turning an otherwise bounded response
/// into an unbounded JSON parse or a multi-megabyte tool payload.
pub const MAX_TOOL_CALL_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum argument bytes retained across all calls in one assistant response.
pub const MAX_TOOL_CALL_TOTAL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: HashMap<u32, PartialToolCall>,
    total_argument_bytes: usize,
    error: Option<ToolError>,
}

impl ToolCallAccumulator {
    pub fn push_delta(&mut self, delta: ToolCallDelta) {
        if self.error.is_some() {
            return;
        }
        let index = delta.index.unwrap_or(0);
        let call = self.calls.entry(index).or_default();

        if let Some(id) = delta.id {
            call.id = Some(id);
        }

        if let Some(FunctionCallDelta { name, arguments }) = delta.function {
            if let Some(name) = name {
                call.name = Some(name);
            }

            if let Some(arguments) = arguments {
                let call_bytes = call.arguments.len().saturating_add(arguments.len());
                let total_bytes = self.total_argument_bytes.saturating_add(arguments.len());
                if call_bytes > MAX_TOOL_CALL_ARGUMENT_BYTES {
                    self.error = Some(ToolError::InvalidArguments {
                        name: call_name(call, index),
                        message: format!(
                            "tool call arguments exceed the {} byte limit",
                            MAX_TOOL_CALL_ARGUMENT_BYTES
                        ),
                    });
                    return;
                }
                if total_bytes > MAX_TOOL_CALL_TOTAL_ARGUMENT_BYTES {
                    self.error = Some(ToolError::InvalidArguments {
                        name: call_name(call, index),
                        message: format!(
                            "tool call arguments exceed the {} byte response limit",
                            MAX_TOOL_CALL_TOTAL_ARGUMENT_BYTES
                        ),
                    });
                    return;
                }
                call.arguments.push_str(&arguments);
                self.total_argument_bytes = total_bytes;
            }
        }
    }

    pub fn finish(self) -> Result<Vec<ToolCall>, ToolError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut calls = self.calls.into_iter().collect::<Vec<_>>();
        calls.sort_by_key(|(index, _)| *index);

        calls
            .into_iter()
            .map(|(index, call)| {
                let id = call.id.unwrap_or_else(|| format!("call_{index}"));
                let name = call.name.ok_or_else(|| ToolError::InvalidArguments {
                    name: id.clone(),
                    message: "missing function name".to_string(),
                })?;
                let arguments = if call.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&call.arguments).map_err(|error| {
                        ToolError::InvalidArguments {
                            name: name.clone(),
                            message: error.to_string(),
                        }
                    })?
                };

                Ok(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect()
    }
}

fn call_name(call: &PartialToolCall, index: u32) -> String {
    call.name
        .clone()
        .or_else(|| call.id.clone())
        .unwrap_or_else(|| format!("call_{index}"))
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::MockEchoTool;
    use serde_json::json;

    #[test]
    fn accumulator_builds_tool_call_from_streaming_deltas() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push_delta(ToolCallDelta {
            index: Some(0),
            id: Some("call_3".to_string()),
            call_type: Some("function".to_string()),
            function: Some(FunctionCallDelta {
                name: Some(MockEchoTool::NAME.to_string()),
                arguments: Some(r#"{"message":"hel"#.to_string()),
            }),
        });
        accumulator.push_delta(ToolCallDelta {
            index: Some(0),
            id: None,
            call_type: None,
            function: Some(FunctionCallDelta {
                name: None,
                arguments: Some(r#"lo"}"#.to_string()),
            }),
        });

        assert_eq!(
            accumulator.finish().unwrap(),
            vec![ToolCall::new(
                "call_3",
                MockEchoTool::NAME,
                json!({"message": "hello"})
            )]
        );
    }

    #[test]
    fn accumulator_rejects_an_oversized_single_call() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push_delta(ToolCallDelta {
            index: Some(0),
            id: Some("oversized".to_string()),
            call_type: Some("function".to_string()),
            function: Some(FunctionCallDelta {
                name: Some(MockEchoTool::NAME.to_string()),
                arguments: Some("x".repeat(MAX_TOOL_CALL_ARGUMENT_BYTES + 1)),
            }),
        });

        let error = accumulator.finish().unwrap_err();
        assert!(matches!(
            &error,
            ToolError::InvalidArguments { message, .. } if message.contains("byte limit")
        ));
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn accumulator_rejects_aggregate_argument_budget() {
        let mut accumulator = ToolCallAccumulator::default();
        let half = MAX_TOOL_CALL_TOTAL_ARGUMENT_BYTES / 2;
        for index in 0..3 {
            accumulator.push_delta(ToolCallDelta {
                index: Some(index),
                id: Some(format!("call-{index}")),
                call_type: Some("function".to_string()),
                function: Some(FunctionCallDelta {
                    name: Some(MockEchoTool::NAME.to_string()),
                    arguments: Some("x".repeat(half)),
                }),
            });
        }

        let error = accumulator.finish().unwrap_err();
        assert!(error.to_string().contains("response limit"));
    }
}
