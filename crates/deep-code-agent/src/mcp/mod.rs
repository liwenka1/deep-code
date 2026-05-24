mod client;
mod config;
mod manager;
mod tools;

#[allow(unused_imports)]
pub use client::{InMemoryMcpClient, connect_client};
#[allow(unused_imports)]
pub use client::{McpPromptDescriptor, McpResourceDescriptor, McpToolDescriptor};
pub use config::{
    McpConfigFile, McpServerConfig, McpServerEntry, McpTransport, default_mcp_config_path,
    load_mcp_config, set_server_enabled, workspace_mcp_config_path,
};
#[allow(unused_imports)]
pub use manager::{
    McpManager, McpServerStatus, McpServerSummary, McpValidationReport, is_mcp_tool_name,
    parse_qualified_tool_name, qualify_tool_name,
};
pub use tools::register_mcp_tools;

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to read MCP config at {path}: {message}")]
    ConfigIo { path: PathBuf, message: String },
    #[error("failed to parse MCP config at {path}: {message}")]
    ConfigParse { path: PathBuf, message: String },
    #[error("invalid MCP config: {message}")]
    InvalidConfig { message: String },
    #[error("unknown MCP server '{name}'")]
    UnknownServer { name: String },
    #[error("MCP server '{name}' is unavailable")]
    ServerUnavailable { name: String },
    #[error("unsupported transport for server '{server}': {transport}")]
    UnsupportedTransport { server: String, transport: String },
    #[error("failed to connect MCP server '{server}': {message}")]
    ConnectFailed { server: String, message: String },
    #[error("MCP protocol error for server '{server}': {message}")]
    Protocol { server: String, message: String },
    #[error("MCP tool '{tool}' not found on server '{server}'")]
    ToolNotFound { server: String, tool: String },
    #[error("invalid qualified MCP tool name '{name}'")]
    InvalidQualifiedTool { name: String },
}
