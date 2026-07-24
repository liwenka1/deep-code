//! `mock_echo`: a trivial tool fixture for exercising the approval + tool loop.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCx, ToolError, ToolOutput};

#[derive(Debug, Clone, Copy)]
pub struct MockEchoTool;

impl MockEchoTool {
    pub const NAME: &'static str = "mock_echo";
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MockEchoParams {
    /// Message to echo back.
    message: String,
}

#[async_trait]
impl Tool for MockEchoTool {
    type Params = MockEchoParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Safely echoes a message to validate the tool loop."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: MockEchoParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("mock_echo: {}", params.message)))
    }
}
