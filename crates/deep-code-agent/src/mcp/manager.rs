use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::McpError;
use super::client::{
    McpClient, McpPromptDescriptor, McpResourceDescriptor, McpToolDescriptor, connect_client,
};
use super::config::{McpConfigFile, McpServerConfig, load_mcp_config};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Disabled,
    Ready,
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSummary {
    pub name: String,
    pub enabled: bool,
    pub status: McpServerStatus,
    pub transport: String,
    pub tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpValidationReport {
    pub valid: bool,
    pub servers: Vec<McpServerSummary>,
    pub errors: Vec<String>,
}

struct ManagedServer {
    config: McpServerConfig,
    client: Option<Arc<dyn McpClient>>,
    last_error: Option<String>,
}

/// Runtime MCP manager: config, connections, and discovery.
pub struct McpManager {
    config_path: Option<std::path::PathBuf>,
    servers: RwLock<HashMap<String, ManagedServer>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config_path: None,
            servers: RwLock::new(HashMap::new()),
        }
    }

    pub fn load_from_workspace(workspace: &Path) -> Result<Self, McpError> {
        let config = load_mcp_config(workspace)?;
        let mut manager = Self::new();
        manager.reload_configs(config.to_server_configs())?;
        Ok(manager)
    }

    pub fn reload_configs(&mut self, configs: Vec<McpServerConfig>) -> Result<(), McpError> {
        let mut servers = HashMap::new();
        for config in configs {
            let name = config.name.clone();
            let mut entry = ManagedServer {
                config: config.clone(),
                client: None,
                last_error: None,
            };
            if config.enabled {
                match connect_client(&config) {
                    Ok(client) => entry.client = Some(Arc::from(client)),
                    Err(error) => entry.last_error = Some(error.to_string()),
                }
            }
            servers.insert(name, entry);
        }
        *self.servers.write().expect("mcp lock") = servers;
        Ok(())
    }

    pub fn register_mock_client(
        &mut self,
        config: McpServerConfig,
        client: Arc<dyn McpClient>,
    ) -> Result<(), McpError> {
        config.validate()?;
        let name = config.name.clone();
        self.servers.write().expect("mcp lock").insert(
            name,
            ManagedServer {
                config,
                client: Some(client),
                last_error: None,
            },
        );
        Ok(())
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), McpError> {
        let mut servers = self.servers.write().expect("mcp lock");
        let entry = servers
            .get_mut(name)
            .ok_or_else(|| McpError::UnknownServer {
                name: name.to_string(),
            })?;
        entry.config.enabled = enabled;
        if enabled {
            match connect_client(&entry.config) {
                Ok(client) => {
                    entry.client = Some(Arc::from(client));
                    entry.last_error = None;
                }
                Err(error) => {
                    entry.client = None;
                    entry.last_error = Some(error.to_string());
                }
            }
        } else {
            entry.client = None;
            entry.last_error = None;
        }
        Ok(())
    }

    pub fn validate(&self) -> McpValidationReport {
        let servers = self.servers.read().expect("mcp lock");
        let mut errors = Vec::new();
        let mut summaries = Vec::new();
        for (name, entry) in servers.iter() {
            if let Err(error) = entry.config.validate() {
                errors.push(format!("{name}: {error}"));
            }
            if entry.config.enabled
                && let Some(error) = &entry.last_error
            {
                errors.push(format!("{name}: {error}"));
            }
            let (tool_count, resource_count, prompt_count) = if let Some(client) = &entry.client {
                (
                    client.list_tools().map(|v| v.len()).unwrap_or(0),
                    client.list_resources().map(|v| v.len()).unwrap_or(0),
                    client.list_prompts().map(|v| v.len()).unwrap_or(0),
                )
            } else {
                (0, 0, 0)
            };
            summaries.push(McpServerSummary {
                name: name.clone(),
                enabled: entry.config.enabled,
                status: server_status(entry),
                transport: format!("{:?}", entry.config.transport).to_ascii_lowercase(),
                tool_count,
                resource_count,
                prompt_count,
            });
        }
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        McpValidationReport {
            valid: errors.is_empty(),
            servers: summaries,
            errors,
        }
    }

    pub fn list_servers(&self) -> Vec<McpServerSummary> {
        self.validate().servers
    }

    pub fn list_tools(&self) -> Vec<McpToolDescriptor> {
        let servers = self.servers.read().expect("mcp lock");
        let mut out = Vec::new();
        for (name, entry) in servers.iter() {
            if !entry.config.enabled {
                continue;
            }
            let Some(client) = &entry.client else {
                continue;
            };
            if let Ok(mut tools) = client.list_tools() {
                for tool in &mut tools {
                    tool.server_name = name.clone();
                }
                out.extend(tools);
            }
        }
        out.sort_by(|left, right| {
            left.server_name
                .cmp(&right.server_name)
                .then_with(|| left.tool_name.cmp(&right.tool_name))
        });
        out
    }

    pub fn list_resources(&self) -> Vec<McpResourceDescriptor> {
        let servers = self.servers.read().expect("mcp lock");
        let mut out = Vec::new();
        for (name, entry) in servers.iter() {
            if !entry.config.enabled {
                continue;
            }
            let Some(client) = &entry.client else {
                continue;
            };
            if let Ok(mut resources) = client.list_resources() {
                for resource in &mut resources {
                    resource.server_name = name.clone();
                }
                out.extend(resources);
            }
        }
        out.sort_by(|left, right| left.uri.cmp(&right.uri));
        out
    }

    pub fn list_prompts(&self) -> Vec<McpPromptDescriptor> {
        let servers = self.servers.read().expect("mcp lock");
        let mut out = Vec::new();
        for (name, entry) in servers.iter() {
            if !entry.config.enabled {
                continue;
            }
            let Some(client) = &entry.client else {
                continue;
            };
            if let Ok(mut prompts) = client.list_prompts() {
                for prompt in &mut prompts {
                    prompt.server_name = name.clone();
                }
                out.extend(prompts);
            }
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        out
    }

    pub fn call_qualified_tool(
        &self,
        qualified_name: &str,
        arguments: Value,
    ) -> Result<Value, McpError> {
        let (server_name, tool_name) = parse_qualified_tool_name(qualified_name)?;
        let client = {
            let servers = self.servers.read().expect("mcp lock");
            let entry = servers
                .get(&server_name)
                .ok_or_else(|| McpError::UnknownServer {
                    name: server_name.clone(),
                })?;
            entry
                .client
                .as_ref()
                .cloned()
                .ok_or_else(|| McpError::ServerUnavailable {
                    name: server_name.clone(),
                })?
        };
        let result = client.call_tool(&tool_name, arguments);
        if matches!(result, Err(McpError::Timeout { .. })) {
            // Poison the slot: a hung server would otherwise cost the full
            // timeout on every subsequent call. Later calls fail fast with
            // ServerUnavailable; doctor/validate report the reason.
            let mut servers = self.servers.write().expect("mcp lock");
            if let Some(entry) = servers.get_mut(&server_name) {
                entry.client = None;
                entry.last_error =
                    Some("call timed out; server marked unavailable".to_string());
            }
        }
        result
    }

    pub fn qualified_tools(&self) -> Vec<(String, McpToolDescriptor)> {
        self.list_tools()
            .into_iter()
            .map(|tool| {
                let qualified = qualify_tool_name(&tool.server_name, &tool.tool_name);
                (qualified, tool)
            })
            .collect()
    }

    pub fn save_config(
        &mut self,
        path: &Path,
        configs: &[McpServerConfig],
    ) -> Result<(), McpError> {
        let mut file = McpConfigFile::default();
        for config in configs {
            file.servers.insert(
                config.name.clone(),
                super::config::McpServerEntry {
                    command: config.command.clone(),
                    args: config.args.clone(),
                    url: config.url.clone(),
                    env: config.env.clone(),
                    enabled: config.enabled,
                    transport: config.transport,
                },
            );
        }
        file.save(path)?;
        self.config_path = Some(path.to_path_buf());
        Ok(())
    }
}

fn server_status(entry: &ManagedServer) -> McpServerStatus {
    if !entry.config.enabled {
        return McpServerStatus::Disabled;
    }
    if entry.client.is_some() {
        McpServerStatus::Ready
    } else {
        McpServerStatus::Failed {
            error: entry
                .last_error
                .clone()
                .unwrap_or_else(|| "client not connected".to_string()),
        }
    }
}

pub fn parse_qualified_tool_name(value: &str) -> Result<(String, String), McpError> {
    let Some(stripped) = value.strip_prefix("mcp__") else {
        return Err(McpError::InvalidQualifiedTool {
            name: value.to_string(),
        });
    };
    let mut split = stripped.splitn(2, "__");
    let server = split
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| McpError::InvalidQualifiedTool {
            name: value.to_string(),
        })?
        .to_string();
    let tool = split
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| McpError::InvalidQualifiedTool {
            name: value.to_string(),
        })?
        .to_string();
    Ok((server, tool))
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

pub fn qualify_tool_name(server: &str, tool: &str) -> String {
    let mut name = format!(
        "mcp__{}__{}",
        sanitize_component(server),
        sanitize_component(tool)
    );
    if name.len() > 64 {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());
        name.truncate(48);
        name.push('_');
        name.push_str(&hash[..12.min(hash.len())]);
    }
    name
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::mcp::client::InMemoryMcpClient;
    use crate::mcp::config::McpTransport;

    fn mock_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some("mock".to_string()),
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            enabled: true,
        }
    }

    #[test]
    fn manager_discovers_and_calls_mock_tools() {
        let client = Arc::new(InMemoryMcpClient::new("mock").with_tool(
            "echo",
            Some("echo tool"),
            json!({"ok": true}),
        ));
        let mut manager = McpManager::new();
        manager
            .register_mock_client(mock_config("mock"), client)
            .unwrap();
        let tools = manager.list_tools();
        assert_eq!(tools.len(), 1);
        let qualified = qualify_tool_name("mock", "echo");
        let result = manager
            .call_qualified_tool(&qualified, json!({"message": "hi"}))
            .unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn parse_qualified_tool_name_roundtrip() {
        let qualified = qualify_tool_name("My Server", "Do Thing");
        let (server, tool) = parse_qualified_tool_name(&qualified).unwrap();
        assert_eq!(server, "my_server");
        assert_eq!(tool, "do_thing");
    }

    #[test]
    fn timeout_poisons_server_slot_for_fast_failure() {
        use crate::mcp::client::{
            McpClient, McpPromptDescriptor, McpResourceDescriptor, McpToolDescriptor,
        };

        struct HangingClient;
        impl McpClient for HangingClient {
            fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError> {
                Ok(Vec::new())
            }
            fn call_tool(&self, _tool: &str, _arguments: Value) -> Result<Value, McpError> {
                Err(McpError::Timeout {
                    server: "mock".to_string(),
                })
            }
            fn list_resources(&self) -> Result<Vec<McpResourceDescriptor>, McpError> {
                Ok(Vec::new())
            }
            fn list_prompts(&self) -> Result<Vec<McpPromptDescriptor>, McpError> {
                Ok(Vec::new())
            }
        }

        let mut manager = McpManager::new();
        manager
            .register_mock_client(mock_config("mock"), Arc::new(HangingClient))
            .unwrap();
        let qualified = qualify_tool_name("mock", "slow");

        let first = manager.call_qualified_tool(&qualified, json!({}));
        assert!(matches!(first, Err(McpError::Timeout { .. })));

        // The slot is poisoned: no more 30s waits, immediate unavailable.
        let second = manager.call_qualified_tool(&qualified, json!({}));
        assert!(matches!(second, Err(McpError::ServerUnavailable { .. })));
    }
}
