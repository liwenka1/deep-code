use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handle::{HandleId, HandleReadOutput, HandleStore};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput};
use crate::workspace_policy::invalid;

pub const HANDLE_READ_TOOL: &str = "handle_read";

const DEFAULT_MAX_CHARS: usize = 12_000;
const HARD_MAX_CHARS: usize = 50_000;
const DEFAULT_HEAD_TAIL_LINES: usize = 50;

#[derive(Clone)]
pub struct HandleReadTool {
    store: Arc<RwLock<HandleStore>>,
}

impl HandleReadTool {
    #[must_use]
    pub fn new(store: Arc<RwLock<HandleStore>>) -> Self {
        Self { store }
    }
}

/// `handle` stays a raw [`Value`]: the model-facing schema is a hand-written
/// `oneOf` (string | selector object) that schemars cannot express faithfully,
/// and `parse_handle_id` implements the matching alias resolution.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandleReadParams {
    handle: Value,
    mode: String,
    lines: Option<u64>,
    start_line: Option<u64>,
    end_line: Option<u64>,
    max_chars: Option<u64>,
}

#[async_trait]
impl Tool for HandleReadTool {
    type Params = HandleReadParams;

    fn name(&self) -> &str {
        HANDLE_READ_TOOL
    }

    fn description(&self) -> &str {
        "Read a bounded projection from a handle returned by sub-agents or other large-output tools. Use mode=summary|head|tail|lines|count."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["handle", "mode"],
            "properties": {
                "handle": {
                    "description": "Handle id string, session_id/name alias, or var_handle object.",
                    "oneOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "name": {"type": "string"},
                                "session_id": {"type": "string"}
                            }
                        }
                    ]
                },
                "mode": {
                    "type": "string",
                    "enum": ["summary", "head", "tail", "lines", "count"]
                },
                "lines": {
                    "type": "integer",
                    "description": "Line count for head/tail modes (default 50)."
                },
                "start_line": {
                    "type": "integer",
                    "description": "1-based start line for lines mode."
                },
                "end_line": {
                    "type": "integer",
                    "description": "1-based inclusive end line for lines mode."
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Hard cap on returned characters (default 12000, max 50000)."
                }
            },
            "additionalProperties": false
        })
    }

    async fn run(&self, params: HandleReadParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let mode = params.mode.as_str();
        let max_chars = params
            .max_chars
            .map(|value| (value as usize).min(HARD_MAX_CHARS))
            .unwrap_or(DEFAULT_MAX_CHARS);

        let store = self
            .store
            .read()
            .map_err(|error| ToolError::ExecutionFailed {
                name: HANDLE_READ_TOOL.to_string(),
                message: error.to_string(),
            })?;
        let handle_id = parse_handle_id(&params.handle, &store)?;

        let output = match mode {
            "summary" => HandleReadOutput {
                mode: mode.to_string(),
                handle_id: handle_id.as_str().to_string(),
                content: None,
                truncated: false,
                count: None,
                summary: store.get_summary(&handle_id),
            },
            "count" => HandleReadOutput {
                mode: mode.to_string(),
                handle_id: handle_id.as_str().to_string(),
                content: None,
                truncated: false,
                count: store.count(&handle_id),
                summary: None,
            },
            "head" => {
                let lines = params.lines.unwrap_or(DEFAULT_HEAD_TAIL_LINES as u64) as usize;
                let (content, truncated) = store
                    .read_head(&handle_id, lines.max(1), max_chars)
                    .ok_or_else(|| missing_handle(&handle_id))?;
                HandleReadOutput {
                    mode: mode.to_string(),
                    handle_id: handle_id.as_str().to_string(),
                    content: Some(content),
                    truncated,
                    count: None,
                    summary: None,
                }
            }
            "tail" => {
                let lines = params.lines.unwrap_or(DEFAULT_HEAD_TAIL_LINES as u64) as usize;
                let (content, truncated) = store
                    .read_tail(&handle_id, lines.max(1), max_chars)
                    .ok_or_else(|| missing_handle(&handle_id))?;
                HandleReadOutput {
                    mode: mode.to_string(),
                    handle_id: handle_id.as_str().to_string(),
                    content: Some(content),
                    truncated,
                    count: None,
                    summary: None,
                }
            }
            "lines" => {
                let start = params.start_line.unwrap_or(1) as usize;
                let end = params.end_line.unwrap_or(start as u64) as usize;
                let (content, truncated) = store
                    .read_lines(&handle_id, start, end, max_chars)
                    .ok_or_else(|| missing_handle(&handle_id))?;
                HandleReadOutput {
                    mode: mode.to_string(),
                    handle_id: handle_id.as_str().to_string(),
                    content: Some(content),
                    truncated,
                    count: None,
                    summary: None,
                }
            }
            other => {
                return Err(invalid(
                    HANDLE_READ_TOOL,
                    format!("unsupported mode '{other}'"),
                ));
            }
        };

        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.handle_id.clone()),
        ))
    }
}

fn parse_handle_id(value: &Value, store: &HandleStore) -> Result<HandleId, ToolError> {
    if let Some(raw) = value.as_str() {
        return store
            .resolve_id(raw)
            .map(Ok)
            .unwrap_or_else(|| Ok(HandleId(raw.to_string())))
            .and_then(|id| {
                if store.get_summary(&id).is_some() {
                    Ok(id)
                } else {
                    Err(missing_handle(&id))
                }
            });
    }

    if let Some(id) = value.get("id").and_then(Value::as_str) {
        let handle = HandleId(id.to_string());
        if store.get_summary(&handle).is_some() {
            return Ok(handle);
        }
    }

    if let Some(name) = value.get("name").and_then(Value::as_str) {
        if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
            let alias = format!("{session_id}/{name}");
            if let Some(id) = store.resolve_id(&alias) {
                return Ok(id);
            }
        }
        let handle = HandleId(name.to_string());
        if store.get_summary(&handle).is_some() {
            return Ok(handle);
        }
    }

    Err(invalid(HANDLE_READ_TOOL, "invalid handle reference"))
}

fn missing_handle(id: &HandleId) -> ToolError {
    ToolError::ExecutionFailed {
        name: HANDLE_READ_TOOL.to_string(),
        message: format!("no payload found for handle {}", id.as_str()),
    }
}

pub fn register_handle_read(
    registry: &mut crate::tool::ToolRegistry,
    store: Arc<RwLock<HandleStore>>,
) {
    registry.register(HandleReadTool::new(store));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{HandleKind, HandleStore};
    use crate::tool::{ErasedTool, ToolCall};

    #[tokio::test]
    async fn handle_read_summary_and_head() {
        let store = Arc::new(RwLock::new(HandleStore::new()));
        let handle_id = {
            let mut guard = store.write().unwrap();
            guard
                .insert_text(
                    "demo",
                    HandleKind::Artifact,
                    "alpha\nbeta\ngamma\n".to_string(),
                    None,
                )
                .id
        };
        let tool = HandleReadTool::new(Arc::clone(&store));

        let summary_call = ToolCall::new(
            "c1",
            HANDLE_READ_TOOL,
            json!({"handle": handle_id.as_str(), "mode": "summary"}),
        );
        let summary = ErasedTool::execute(&tool, &summary_call, &ToolCx::default())
            .await
            .unwrap();
        assert!(summary.content.contains("byte_len"));

        let head_call = ToolCall::new(
            "c2",
            HANDLE_READ_TOOL,
            json!({"handle": handle_id.as_str(), "mode": "head", "lines": 1}),
        );
        let head = ErasedTool::execute(&tool, &head_call, &ToolCx::default())
            .await
            .unwrap();
        assert!(head.content.contains("alpha"));
    }
}
