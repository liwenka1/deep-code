//! Reassembly of streamed tool-call deltas into complete [`ToolCall`]s.

use std::collections::HashMap;

use serde_json::Value;

use crate::model::{FunctionCallDelta, ToolCallDelta};

use super::{ToolCall, ToolError};

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: HashMap<u32, PartialToolCall>,
}

impl ToolCallAccumulator {
    pub fn push_delta(&mut self, delta: ToolCallDelta) {
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
                call.arguments.push_str(&arguments);
            }
        }
    }

    pub fn finish(self) -> Result<Vec<ToolCall>, ToolError> {
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
}
