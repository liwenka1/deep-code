use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::handle::{HandleStore, VarHandle};
use crate::rlm::session::{RlmError, RlmManager, derive_session_name};
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, run_blocking};
use crate::workspace_policy::{WorkspacePolicy, invalid};

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

#[derive(Clone)]
pub struct RlmOpenTool {
    services: Arc<RlmServices>,
}

impl RlmOpenTool {
    const NAME: &'static str = "rlm_open";

    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }

    fn open_sync(&self, params: RlmOpenParams) -> Result<ToolOutput, ToolError> {
        let file_path = params.file_path.as_deref();
        let content = params.content.as_deref();
        let source_count = [file_path.is_some(), content.is_some()]
            .into_iter()
            .filter(|present| *present)
            .count();
        if source_count != 1 {
            return Err(invalid(
                Self::NAME,
                "provide exactly one of `file_path` or `content`",
            ));
        }

        let (body, source_type, source_hint) = if let Some(path) = file_path {
            let policy = WorkspacePolicy::new(&self.services.workspace)?;
            let resolved = policy.resolve_existing(path, Self::NAME)?;
            let body =
                std::fs::read_to_string(&resolved).map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
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
            return Err(invalid(Self::NAME, "input is empty after loading"));
        }

        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| derive_session_name(source_hint.as_deref()));

        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
                    message: error.to_string(),
                })?;
        let info = manager
            .open(name.clone(), body, source_type)
            .map_err(rlm_error)?;

        Ok(ToolOutput::text(
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RlmOpenParams {
    /// Stable session name
    name: Option<String>,
    /// Workspace-relative file to load
    file_path: Option<String>,
    /// Inline text payload
    content: Option<String>,
}

#[async_trait]
impl Tool for RlmOpenTool {
    type Params = RlmOpenParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Open a named analysis session over inline content or a workspace file. Returns session metadata; large payloads stay in the session runtime."
    }

    async fn run(&self, params: RlmOpenParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.open_sync(params)).await
    }
}

#[derive(Clone)]
pub struct RlmEvalTool {
    services: Arc<RlmServices>,
}

impl RlmEvalTool {
    const NAME: &'static str = "rlm_eval";

    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }

    fn eval_sync(&self, params: RlmEvalParams) -> Result<ToolOutput, ToolError> {
        let name = params.name.as_str();
        let code = params.code.as_str();
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
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
        Ok(ToolOutput::text(payload.to_string()))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RlmEvalParams {
    /// Session name from rlm_open
    name: String,
    /// Newline-separated analysis commands
    code: String,
}

#[async_trait]
impl Tool for RlmEvalTool {
    type Params = RlmEvalParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run bounded analysis commands against an open RLM session. Large stdout is stored as a handle instead of flooding the parent transcript."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: RlmEvalParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.eval_sync(params)).await
    }
}

#[derive(Clone)]
pub struct RlmConfigureTool {
    services: Arc<RlmServices>,
}

impl RlmConfigureTool {
    const NAME: &'static str = "rlm_configure";

    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }

    fn configure_sync(&self, params: RlmConfigureParams) -> Result<ToolOutput, ToolError> {
        let name = params.name.as_str();
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
                    message: error.to_string(),
                })?;
        let config = manager
            .configure(name, params.max_inline_chars, params.grep_max_matches)
            .map_err(rlm_error)?;
        Ok(ToolOutput::text(
            json!({"name": name, "config": config}).to_string(),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RlmConfigureParams {
    name: String,
    max_inline_chars: Option<usize>,
    grep_max_matches: Option<usize>,
}

#[async_trait]
impl Tool for RlmConfigureTool {
    type Params = RlmConfigureParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Adjust RLM session feedback limits such as inline char cap and grep match cap."
    }

    async fn run(
        &self,
        params: RlmConfigureParams,
        _cx: &ToolCx,
    ) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.configure_sync(params)).await
    }
}

#[derive(Clone)]
pub struct RlmCloseTool {
    services: Arc<RlmServices>,
}

impl RlmCloseTool {
    const NAME: &'static str = "rlm_close";

    pub fn new(services: Arc<RlmServices>) -> Self {
        Self { services }
    }

    fn close_sync(&self, params: RlmCloseParams) -> Result<ToolOutput, ToolError> {
        let name = params.name.as_str();
        let mut manager =
            self.services
                .manager
                .write()
                .map_err(|error| ToolError::ExecutionFailed {
                    name: Self::NAME.to_string(),
                    message: error.to_string(),
                })?;
        let info = manager.close(name).map_err(rlm_error)?;
        Ok(ToolOutput::text(
            json!({"name": info.name, "closed": true, "eval_count": info.eval_count}).to_string(),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RlmCloseParams {
    name: String,
}

#[async_trait]
impl Tool for RlmCloseTool {
    type Params = RlmCloseParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Close a named RLM session and purge session-owned handles."
    }

    async fn run(&self, params: RlmCloseParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.close_sync(params)).await
    }
}

pub fn register_rlm_tools(registry: &mut crate::tool::ToolRegistry, services: Arc<RlmServices>) {
    registry.register(RlmOpenTool::new(Arc::clone(&services)));
    registry.register(RlmEvalTool::new(Arc::clone(&services)));
    registry.register(RlmConfigureTool::new(Arc::clone(&services)));
    registry.register(RlmCloseTool::new(services));
}
