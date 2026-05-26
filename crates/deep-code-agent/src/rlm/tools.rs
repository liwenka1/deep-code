use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::handle::{HandleStore, VarHandle};
use crate::rlm::session::{RlmError, RlmManager, derive_session_name};
use crate::tool::{Tool, ToolCall, ToolError, ToolResult};
use crate::workspace_policy::{WorkspacePolicy, invalid, optional_str, required_str};

pub const RLM_TOOL_NAMES: [&str; 4] = ["rlm_open", "rlm_eval", "rlm_configure", "rlm_close"];

pub fn is_rlm_tool(name: &str) -> bool {
    RLM_TOOL_NAMES.contains(&name)
}

fn rlm_error(error: RlmError) -> ToolError {
    ToolError::ExecutionFailed {
        name: "rlm".to_string(),
        message: error.to_string(),
    }
}

pub struct RlmServices {
    pub manager: Arc<RwLock<RlmManager>>,
    pub workspace: PathBuf,
}

impl RlmServices {
    pub fn new(handle_store: Arc<RwLock<HandleStore>>, workspace: PathBuf) -> Self {
        Self {
            manager: Arc::new(RwLock::new(RlmManager::new(handle_store))),
            workspace,
        }
    }
}

pub struct RlmOpenTool {
    services: Arc<RlmServices>,
}

impl RlmOpenTool {
    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }
}

impl Tool for RlmOpenTool {
    fn spec(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::new(
            "rlm_open",
            "Open a named analysis session over inline content or a workspace file. Returns session metadata; large payloads stay in the session runtime.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Stable session name"},
                    "file_path": {"type": "string", "description": "Workspace-relative file to load"},
                    "content": {"type": "string", "description": "Inline text payload"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let file_path = optional_str(&call.arguments, "file_path");
        let content = optional_str(&call.arguments, "content");
        let source_count = [file_path.is_some(), content.is_some()]
            .into_iter()
            .filter(|present| *present)
            .count();
        if source_count != 1 {
            return Err(invalid(
                "rlm_open",
                "provide exactly one of `file_path` or `content`",
            ));
        }

        let (body, source_type, source_hint) = if let Some(path) = file_path {
            let policy = WorkspacePolicy::new(&self.services.workspace)?;
            let resolved = policy.resolve_existing(path, "rlm_open")?;
            let body =
                std::fs::read_to_string(&resolved).map_err(|error| ToolError::ExecutionFailed {
                    name: "rlm_open".to_string(),
                    message: format!("failed to read {}: {error}", resolved.display()),
                })?;
            (
                body,
                "file".to_string(),
                Some(policy.relative_display(&resolved)),
            )
        } else {
            (
                content.unwrap_or_default().to_string(),
                "inline".to_string(),
                None,
            )
        };

        if body.trim().is_empty() {
            return Err(invalid("rlm_open", "input is empty after loading"));
        }

        let name = optional_str(&call.arguments, "name")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| derive_session_name(source_hint.as_deref()));

        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: "rlm_open".to_string(),
                    message: error.to_string(),
                })?;
        let info = manager
            .open(name.clone(), body, source_type)
            .map_err(rlm_error)?;

        Ok(ToolResult::success(
            &call.id,
            "rlm_open",
            json!({
                "name": info.name,
                "id": info.id,
                "source_type": info.source_type,
                "byte_len": info.byte_len,
                "line_count": info.line_count,
                "runtime": "analysis_v1",
                "commands": ["stats", "head N", "tail N", "lines START END", "grep PATTERN", "set NAME VALUE", "get NAME", "peek START END"]
            })
            .to_string(),
        ))
    }
}

pub struct RlmEvalTool {
    services: Arc<RlmServices>,
}

impl RlmEvalTool {
    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }
}

impl Tool for RlmEvalTool {
    fn spec(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::new(
            "rlm_eval",
            "Run bounded analysis commands against an open RLM session. Large stdout is stored as a handle instead of flooding the parent transcript.",
            json!({
                "type": "object",
                "required": ["name", "code"],
                "properties": {
                    "name": {"type": "string", "description": "Session name from rlm_open"},
                    "code": {"type": "string", "description": "Newline-separated analysis commands"}
                },
                "additionalProperties": false
            }),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let name = required_str(&call.arguments, "name", "rlm_eval")?;
        let code = required_str(&call.arguments, "code", "rlm_eval")?;
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: "rlm_eval".to_string(),
                    message: error.to_string(),
                })?;
        let output = manager.eval(name, code).map_err(rlm_error)?;
        let payload = if output.stored_handle {
            let handle_id = output.handle_id.clone().unwrap_or_default();
            json!({
                "name": name,
                "stored": true,
                "handle": VarHandle::from_summary(
                    &crate::handle::HandleSummary {
                        id: crate::handle::HandleId(handle_id.clone()),
                        kind: crate::handle::HandleKind::RlmResult,
                        summary: format!("rlm eval output ({})", output.byte_len),
                        byte_len: output.byte_len,
                        line_count: output.line_count,
                        session_owner: Some(name.to_string()),
                    },
                    name,
                ),
                "byte_len": output.byte_len,
                "line_count": output.line_count,
            })
        } else {
            json!({
                "name": name,
                "stored": false,
                "output": output.inline,
                "byte_len": output.byte_len,
                "line_count": output.line_count,
            })
        };
        Ok(ToolResult::success(
            &call.id,
            "rlm_eval",
            payload.to_string(),
        ))
    }
}

pub struct RlmConfigureTool {
    services: Arc<RlmServices>,
}

impl RlmConfigureTool {
    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }
}

impl Tool for RlmConfigureTool {
    fn spec(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::new(
            "rlm_configure",
            "Adjust RLM session feedback limits such as inline char cap and grep match cap.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string"},
                    "max_inline_chars": {"type": "integer"},
                    "grep_max_matches": {"type": "integer"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let name = required_str(&call.arguments, "name", "rlm_configure")?;
        let max_inline = call
            .arguments
            .get("max_inline_chars")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let grep_max = call
            .arguments
            .get("grep_max_matches")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: "rlm_configure".to_string(),
                    message: error.to_string(),
                })?;
        let config = manager
            .configure(name, max_inline, grep_max)
            .map_err(rlm_error)?;
        Ok(ToolResult::success(
            &call.id,
            "rlm_configure",
            json!({"name": name, "config": config}).to_string(),
        ))
    }
}

pub struct RlmCloseTool {
    services: Arc<RlmServices>,
}

impl RlmCloseTool {
    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }
}

impl Tool for RlmCloseTool {
    fn spec(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::new(
            "rlm_close",
            "Close a named RLM session and purge session-owned handles.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string"}
                },
                "additionalProperties": false
            }),
            false,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let name = required_str(&call.arguments, "name", "rlm_close")?;
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: "rlm_close".to_string(),
                    message: error.to_string(),
                })?;
        let info = manager.close(name).map_err(rlm_error)?;
        Ok(ToolResult::success(
            &call.id,
            "rlm_close",
            json!({"name": info.name, "closed": true, "eval_count": info.eval_count}).to_string(),
        ))
    }
}

pub fn register_rlm_tools(registry: &mut crate::tool::ToolRegistry, services: Arc<RlmServices>) {
    registry.register(RlmOpenTool::new(Arc::clone(&services)));
    registry.register(RlmEvalTool::new(Arc::clone(&services)));
    registry.register(RlmConfigureTool::new(Arc::clone(&services)));
    registry.register(RlmCloseTool::new(services));
}
