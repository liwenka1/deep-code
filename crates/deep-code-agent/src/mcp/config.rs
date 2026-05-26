use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::McpError;

/// Transport for an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

/// One MCP server entry from configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Top-level MCP configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfigFile {
    #[serde(default, alias = "mcpServers")]
    pub servers: HashMap<String, McpServerEntry>,
}

/// Cursor/Claude-compatible nested server definition.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpServerEntry {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub transport: McpTransport,
}

impl McpConfigFile {
    pub fn load(path: &Path) -> Result<Self, McpError> {
        let raw = fs::read_to_string(path).map_err(|error| McpError::ConfigIo {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        serde_json::from_str(&raw).map_err(|error| McpError::ConfigParse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), McpError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| McpError::ConfigIo {
                path: parent.to_path_buf(),
                message: error.to_string(),
            })?;
        }
        let encoded =
            serde_json::to_string_pretty(self).map_err(|error| McpError::ConfigParse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        fs::write(path, encoded).map_err(|error| McpError::ConfigIo {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }

    pub fn to_server_configs(&self) -> Vec<McpServerConfig> {
        let mut configs = Vec::new();
        for (name, entry) in &self.servers {
            configs.push(entry.to_config(name));
        }
        configs.sort_by(|left, right| left.name.cmp(&right.name));
        configs
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), McpError> {
        let entry = self
            .servers
            .get_mut(name)
            .ok_or_else(|| McpError::UnknownServer {
                name: name.to_string(),
            })?;
        entry.enabled = enabled;
        Ok(())
    }
}

impl McpServerEntry {
    pub fn to_config(&self, name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: self.transport,
            command: self.command.clone(),
            args: self.args.clone(),
            url: self.url.clone(),
            env: self.env.clone(),
            enabled: self.enabled,
        }
    }
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), McpError> {
        if self.name.trim().is_empty() {
            return Err(McpError::InvalidConfig {
                message: "server name must not be empty".to_string(),
            });
        }
        match self.transport {
            McpTransport::Stdio => {
                if self
                    .command
                    .as_ref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return Err(McpError::InvalidConfig {
                        message: format!(
                            "stdio server '{}' requires a non-empty command",
                            self.name
                        ),
                    });
                }
            }
            McpTransport::Http => {
                if self
                    .url
                    .as_ref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return Err(McpError::InvalidConfig {
                        message: format!("http server '{}' requires a url", self.name),
                    });
                }
            }
        }
        Ok(())
    }
}

#[must_use]
pub fn default_mcp_config_path() -> PathBuf {
    home_dir()
        .map(|home| home.join(".deep-code").join("mcp.json"))
        .unwrap_or_else(|| PathBuf::from(".deep-code/mcp.json"))
}

#[must_use]
pub fn workspace_mcp_config_path(workspace: &Path) -> PathBuf {
    workspace.join(".deep-code").join("mcp.json")
}

pub fn load_mcp_config(workspace: &Path) -> Result<McpConfigFile, McpError> {
    merged_mcp_config_from_paths(workspace, &default_mcp_config_path())
}

/// Toggle a server in the config layer that owns its definition.
///
/// Workspace entries win over global when the same name exists in both.
/// Returns the path that was written.
pub fn set_server_enabled(
    workspace: &Path,
    name: &str,
    enabled: bool,
) -> Result<PathBuf, McpError> {
    set_server_enabled_with_global_path(workspace, &default_mcp_config_path(), name, enabled)
}

fn set_server_enabled_with_global_path(
    workspace: &Path,
    global_path: &Path,
    name: &str,
    enabled: bool,
) -> Result<PathBuf, McpError> {
    let merged = merged_mcp_config_from_paths(workspace, global_path)?;
    if !merged.servers.contains_key(name) {
        return Err(McpError::UnknownServer {
            name: name.to_string(),
        });
    }

    let (local_path, global_path_buf, mut local, mut global) =
        load_mcp_layer_configs(workspace, global_path)?;

    match config_layer_for_server(&local, &global, name)? {
        McpConfigLayer::Workspace => {
            local.set_enabled(name, enabled)?;
            local.save(&local_path)?;
            Ok(local_path)
        }
        McpConfigLayer::Global => {
            global.set_enabled(name, enabled)?;
            global.save(&global_path_buf)?;
            Ok(global_path_buf)
        }
    }
}

fn merged_mcp_config_from_paths(
    workspace: &Path,
    global_path: &Path,
) -> Result<McpConfigFile, McpError> {
    let mut merged = McpConfigFile::default();
    if global_path.is_file() {
        merged = merge_configs(merged, McpConfigFile::load(global_path)?);
    }
    let local_path = workspace_mcp_config_path(workspace);
    if local_path.is_file() {
        merged = merge_configs(merged, McpConfigFile::load(&local_path)?);
    }
    Ok(merged)
}

fn load_mcp_layer_configs(
    workspace: &Path,
    global_path: &Path,
) -> Result<(PathBuf, PathBuf, McpConfigFile, McpConfigFile), McpError> {
    let local_path = workspace_mcp_config_path(workspace);
    let local = if local_path.is_file() {
        McpConfigFile::load(&local_path)?
    } else {
        McpConfigFile::default()
    };
    let global_path_buf = global_path.to_path_buf();
    let global = if global_path.is_file() {
        McpConfigFile::load(global_path)?
    } else {
        McpConfigFile::default()
    };
    Ok((local_path, global_path_buf, local, global))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpConfigLayer {
    Workspace,
    Global,
}

fn config_layer_for_server(
    local: &McpConfigFile,
    global: &McpConfigFile,
    name: &str,
) -> Result<McpConfigLayer, McpError> {
    if local.servers.contains_key(name) {
        Ok(McpConfigLayer::Workspace)
    } else if global.servers.contains_key(name) {
        Ok(McpConfigLayer::Global)
    } else {
        Err(McpError::UnknownServer {
            name: name.to_string(),
        })
    }
}

fn merge_configs(base: McpConfigFile, overlay: McpConfigFile) -> McpConfigFile {
    let mut servers = base.servers;
    servers.extend(overlay.servers);
    McpConfigFile { servers }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_cursor_style_mcp_json() {
        let raw = r#"{
  "mcpServers": {
    "mock": {
      "command": "node",
      "args": ["server.js"],
      "enabled": true
    }
  }
}"#;
        let config: McpConfigFile = serde_json::from_str(raw).unwrap();
        let servers = config.to_server_configs();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "mock");
        assert_eq!(servers[0].command.as_deref(), Some("node"));
    }

    #[test]
    fn validate_stdio_requires_command() {
        let config = McpServerConfig {
            name: "bad".to_string(),
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            enabled: true,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn workspace_overlay_overrides_global_name() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.json");
        let local = tmp.path().join("local.json");
        McpConfigFile {
            servers: HashMap::from([(
                "shared".to_string(),
                McpServerEntry {
                    command: Some("global".to_string()),
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        }
        .save(&global)
        .unwrap();
        McpConfigFile {
            servers: HashMap::from([(
                "shared".to_string(),
                McpServerEntry {
                    command: Some("local".to_string()),
                    enabled: false,
                    ..McpServerEntry::default()
                },
            )]),
        }
        .save(&local)
        .unwrap();

        let mut merged = McpConfigFile::load(&global).unwrap();
        merged = merge_configs(merged, McpConfigFile::load(&local).unwrap());
        let shared = merged.servers.get("shared").unwrap();
        assert_eq!(shared.command.as_deref(), Some("local"));
        assert!(!shared.enabled);
    }

    #[test]
    fn config_layer_for_server_prefers_workspace_definition() {
        let local = McpConfigFile {
            servers: HashMap::from([(
                "shared".to_string(),
                McpServerEntry {
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        };
        let global = McpConfigFile {
            servers: HashMap::from([(
                "shared".to_string(),
                McpServerEntry {
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        };
        assert_eq!(
            config_layer_for_server(&local, &global, "shared").unwrap(),
            McpConfigLayer::Workspace
        );
    }

    #[test]
    fn config_layer_for_server_uses_global_when_workspace_file_is_empty() {
        let local = McpConfigFile::default();
        let global = McpConfigFile {
            servers: HashMap::from([(
                "mock".to_string(),
                McpServerEntry {
                    command: Some("node".to_string()),
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        };
        assert_eq!(
            config_layer_for_server(&local, &global, "mock").unwrap(),
            McpConfigLayer::Global
        );
    }

    #[test]
    fn set_server_enabled_updates_workspace_owned_server() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let local = workspace_mcp_config_path(workspace);
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        McpConfigFile {
            servers: HashMap::from([(
                "mock".to_string(),
                McpServerEntry {
                    command: Some("node".to_string()),
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        }
        .save(&local)
        .unwrap();

        let written = set_server_enabled(workspace, "mock", false).unwrap();
        assert_eq!(written, local);
        let updated = McpConfigFile::load(&written).unwrap();
        assert!(!updated.servers.get("mock").unwrap().enabled);
    }

    #[test]
    fn set_server_enabled_updates_global_when_workspace_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(workspace.join(".deep-code")).unwrap();
        let global_path = tmp.path().join("global-mcp.json");
        let local_path = workspace_mcp_config_path(&workspace);

        McpConfigFile {
            servers: HashMap::from([(
                "mock".to_string(),
                McpServerEntry {
                    command: Some("node".to_string()),
                    enabled: true,
                    ..McpServerEntry::default()
                },
            )]),
        }
        .save(&global_path)
        .unwrap();
        McpConfigFile::default().save(&local_path).unwrap();

        let written =
            set_server_enabled_with_global_path(&workspace, &global_path, "mock", false).unwrap();
        assert_eq!(written, global_path);

        let updated = McpConfigFile::load(&global_path).unwrap();
        assert!(!updated.servers.get("mock").unwrap().enabled);

        let local_after = McpConfigFile::load(&local_path).unwrap();
        assert!(local_after.servers.is_empty());
    }
}
