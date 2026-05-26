use std::sync::{Arc, RwLock};

use serde_json::json;

use crate::tool::{Tool, ToolCall, ToolError, ToolRegistry, ToolResult, ToolSpec};

use super::client::McpToolDescriptor;
use super::manager::McpManager;

struct McpDynamicTool {
    manager: Arc<RwLock<McpManager>>,
    descriptor: McpToolDescriptor,
    qualified_name: String,
}

impl McpDynamicTool {
    fn new(
        manager: Arc<RwLock<McpManager>>,
        qualified_name: String,
        descriptor: McpToolDescriptor,
    ) -> Self {
        Self {
            manager,
            descriptor,
            qualified_name,
        }
    }
}

impl Tool for McpDynamicTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.qualified_name.clone(),
            self.descriptor.description.clone().unwrap_or_else(|| {
                format!(
                    "MCP tool {}::{}",
                    self.descriptor.server_name, self.descriptor.tool_name
                )
            }),
            self.descriptor.input_schema.clone(),
            true,
        )
    }

    fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let arguments = if call.arguments.is_null() {
            json!({})
        } else {
            call.arguments.clone()
        };
        let manager = self.manager.read().expect("mcp lock");
        match manager.call_qualified_tool(&self.qualified_name, arguments) {
            Ok(value) => {
                let rendered =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                Ok(ToolResult::success(&call.id, &call.name, rendered))
            }
            Err(error) => Err(ToolError::ExecutionFailed {
                name: call.name.clone(),
                message: error.to_string(),
            }),
        }
    }
}

pub fn register_mcp_tools(registry: &mut ToolRegistry, manager: Arc<RwLock<McpManager>>) {
    let qualified_tools = manager.read().expect("mcp lock").qualified_tools();
    for (qualified_name, descriptor) in qualified_tools {
        registry.register(McpDynamicTool::new(
            Arc::clone(&manager),
            qualified_name,
            descriptor,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use serde_json::json;

    use super::*;
    use crate::mcp::client::InMemoryMcpClient;
    use crate::mcp::config::{McpServerConfig, McpTransport};
    use crate::mcp::manager::qualify_tool_name;
    use crate::tool::{ApprovalDecision, ToolCall, ToolRegistry, ToolRunOutcome};

    #[test]
    fn mcp_tool_runs_through_registry_with_approval() {
        let client = Arc::new(InMemoryMcpClient::new("mock").with_tool(
            "ping",
            Some("ping"),
            json!({"pong": true}),
        ));
        let mut manager = McpManager::new();
        manager
            .register_mock_client(
                McpServerConfig {
                    name: "mock".to_string(),
                    transport: McpTransport::Stdio,
                    command: Some("mock".to_string()),
                    args: Vec::new(),
                    url: None,
                    env: HashMap::new(),
                    enabled: true,
                },
                client,
            )
            .unwrap();
        let manager = Arc::new(RwLock::new(manager));
        let mut registry = ToolRegistry::new();
        register_mcp_tools(&mut registry, Arc::clone(&manager));
        let qualified = qualify_tool_name("mock", "ping");
        let call = ToolCall::new("call_1", qualified, json!({}));
        let pending = registry.run_tool_call(call.clone(), None).unwrap();
        let ToolRunOutcome::ApprovalRequired { .. } = pending else {
            panic!("expected approval");
        };
        let outcome = registry
            .run_tool_call(call, Some(ApprovalDecision::Approved))
            .unwrap();
        let ToolRunOutcome::Result { result } = outcome else {
            panic!("expected result");
        };
        assert!(result.content.contains("pong"));
    }
}
