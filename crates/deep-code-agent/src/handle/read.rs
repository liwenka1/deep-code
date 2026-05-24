use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::handle::{HandleId, HandleReadOutput, HandleStore};
use crate::tool::{Tool, ToolCall, ToolError, ToolResult};
use crate::workspace_policy::{invalid, optional_str, optional_u64};

pub const HANDLE_READ_TOOL: &str = "handle_read";

const DEFAULT_MAX_CHARS: usize = 12_000;
const HARD_MAX_CHARS: usize = 50_000;
const DEFAULT_HEAD_TAIL_LINES: usize = 50;

pub struct HandleReadTool {
    store: Arc<RwLock<HandleStore>>,
}

impl HandleReadTool {
    #[must_use]
    pub fn new(store: Arc<RwLock<HandleStore>>) -> Self {
        Self { store }
    }
}

impl Tool for HandleReadTool {
    fn spec(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::new(
            HANDLE_READ_TOOL,
            "Read a bounded projection from a handle returned by sub-agents, RLM sessions, or other large-output tools. Use mode=summary|head|tail|lines|count.",
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
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let handle_value = call
            .arguments
            .get("handle")
            .ok_or_else(|| invalid(HANDLE_READ_TOOL, "missing handle"))?;
        let mode = optional_str(&call.arguments, "mode")
            .ok_or_else(|| invalid(HANDLE_READ_TOOL, "missing mode"))?;
        let max_chars = call
            .arguments
            .get("max_chars")
            .and_then(Value::as_u64)
            .map(|value| (value as usize).min(HARD_MAX_CHARS))
            .unwrap_or(DEFAULT_MAX_CHARS);

        let store = self.store.read().map_err(|error| ToolError::ExecutionFailed {
            name: HANDLE_READ_TOOL.to_string(),
            message: error.to_string(),
        })?;
        let handle_id = parse_handle_id(handle_value, &store)?;

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
                let lines = optional_u64(&call.arguments, "lines", DEFAULT_HEAD_TAIL_LINES as u64, HANDLE_READ_TOOL)?
                    as usize;
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
                let lines = optional_u64(&call.arguments, "lines", DEFAULT_HEAD_TAIL_LINES as u64, HANDLE_READ_TOOL)?
                    as usize;
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
                let start = optional_u64(&call.arguments, "start_line", 1, HANDLE_READ_TOOL)? as usize;
                let end = optional_u64(&call.arguments, "end_line", start as u64, HANDLE_READ_TOOL)? as usize;
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
                    &format!("unsupported mode '{other}'"),
                ));
            }
        };

        Ok(ToolResult::success(
            &call.id,
            HANDLE_READ_TOOL,
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

pub fn register_handle_read(registry: &mut crate::tool::ToolRegistry, store: Arc<RwLock<HandleStore>>) {
    registry.register(HandleReadTool::new(store));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{HandleKind, HandleStore};

    #[test]
    fn handle_read_summary_and_head() {
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
        let summary = tool.execute(&summary_call).unwrap();
        assert!(summary.content.contains("byte_len"));

        let head_call = ToolCall::new(
            "c2",
            HANDLE_READ_TOOL,
            json!({"handle": handle_id.as_str(), "mode": "head", "lines": 1}),
        );
        let head = tool.execute(&head_call).unwrap();
        assert!(head.content.contains("alpha"));
    }
}
