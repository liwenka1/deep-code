use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::McpError;
use super::config::{McpServerConfig, McpTransport};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Minimal MCP client surface used by the agent runtime.
pub trait McpClient: Send + Sync {
    fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError>;
    fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, McpError>;
    fn list_resources(&self) -> Result<Vec<McpResourceDescriptor>, McpError>;
    fn list_prompts(&self) -> Result<Vec<McpPromptDescriptor>, McpError>;
}

/// In-memory MCP client for tests and mock servers.
#[derive(Debug, Default)]
pub struct InMemoryMcpClient {
    server_name: String,
    tools: HashMap<String, (Option<String>, Value)>,
    resources: HashMap<String, Option<String>>,
    prompts: HashMap<String, (Option<String>, Value)>,
}

impl InMemoryMcpClient {
    #[must_use]
    pub fn new(server_name: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_tool(
        mut self,
        name: impl Into<String>,
        description: Option<&str>,
        sample_result: Value,
    ) -> Self {
        self.tools.insert(
            name.into(),
            (description.map(str::to_string), sample_result),
        );
        self
    }

    #[must_use]
    pub fn with_resource(mut self, uri: impl Into<String>, description: Option<&str>) -> Self {
        self.resources
            .insert(uri.into(), description.map(str::to_string));
        self
    }

    #[must_use]
    pub fn with_prompt(
        mut self,
        name: impl Into<String>,
        description: Option<&str>,
        arguments: Value,
    ) -> Self {
        self.prompts
            .insert(name.into(), (description.map(str::to_string), arguments));
        self
    }
}

impl McpClient for InMemoryMcpClient {
    fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError> {
        let mut tools: Vec<_> = self
            .tools
            .iter()
            .map(|(name, (description, schema))| McpToolDescriptor {
                server_name: self.server_name.clone(),
                tool_name: name.clone(),
                description: description.clone(),
                input_schema: schema.clone(),
            })
            .collect();
        tools.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        Ok(tools)
    }

    fn call_tool(&self, tool_name: &str, _arguments: Value) -> Result<Value, McpError> {
        self.tools
            .get(tool_name)
            .map(|(_, result)| result.clone())
            .ok_or_else(|| McpError::ToolNotFound {
                server: self.server_name.clone(),
                tool: tool_name.to_string(),
            })
    }

    fn list_resources(&self) -> Result<Vec<McpResourceDescriptor>, McpError> {
        let mut resources: Vec<_> = self
            .resources
            .iter()
            .map(|(uri, description)| McpResourceDescriptor {
                server_name: self.server_name.clone(),
                uri: uri.clone(),
                description: description.clone(),
            })
            .collect();
        resources.sort_by(|left, right| left.uri.cmp(&right.uri));
        Ok(resources)
    }

    fn list_prompts(&self) -> Result<Vec<McpPromptDescriptor>, McpError> {
        let mut prompts: Vec<_> = self
            .prompts
            .iter()
            .map(|(name, (description, arguments))| McpPromptDescriptor {
                server_name: self.server_name.clone(),
                name: name.clone(),
                description: description.clone(),
                arguments: arguments.clone(),
            })
            .collect();
        prompts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(prompts)
    }
}

/// Hard ceiling for one MCP request/response round trip (initialize
/// included). A hung server costs at most this much once — the manager then
/// poisons the server slot so later calls fail fast.
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Blocking stdio MCP client (initialize + tools/list + tools/call).
pub struct StdioMcpClient {
    inner: Arc<Mutex<StdioMcpSession>>,
}

/// Lines from the child's stdout, pumped by a dedicated reader thread so
/// response waits can time out even when the server emits nothing at all
/// (a plain blocking `read_line` would hang forever).
enum ReaderEvent {
    Line(String),
    Eof,
    Error(String),
}

struct StdioMcpSession {
    server_name: String,
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<ReaderEvent>,
    next_id: u64,
}

impl StdioMcpClient {
    pub fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        if config.transport != McpTransport::Stdio {
            return Err(McpError::UnsupportedTransport {
                server: config.name.clone(),
                transport: format!("{:?}", config.transport),
            });
        }
        let command = config
            .command
            .as_ref()
            .ok_or_else(|| McpError::InvalidConfig {
                message: format!("stdio server '{}' missing command", config.name),
            })?;
        let mut command_builder = Command::new(command);
        command_builder
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Strip inherited provider/runtime secrets before applying the server's
        // own env, so a third-party MCP server can't read the key out of its
        // environment. An explicit key in `config.env` (user intent) still wins.
        for var in crate::config::SUBPROCESS_SECRET_ENV {
            command_builder.env_remove(var);
        }
        command_builder.envs(&config.env);
        let mut child = command_builder
            .spawn()
            .map_err(|error| McpError::ConnectFailed {
                server: config.name.clone(),
                message: error.to_string(),
            })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::ConnectFailed {
            server: config.name.clone(),
            message: "missing stdout".to_string(),
        })?;
        let (line_tx, line_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = line_tx.send(ReaderEvent::Eof);
                        break;
                    }
                    Ok(_) => {
                        if line_tx.send(ReaderEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.send(ReaderEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
        });
        let mut session = StdioMcpSession {
            server_name: config.name.clone(),
            child,
            lines: line_rx,
            next_id: REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        };
        session.initialize()?;
        session.notify_initialized()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(session)),
        })
    }
}

impl McpClient for StdioMcpClient {
    fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError> {
        let mut session = self.inner.lock().expect("stdio mcp lock");
        let result = session.request("tools/list", json!({}))?;
        parse_tool_list(&session.server_name, result)
    }

    fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, McpError> {
        let mut session = self.inner.lock().expect("stdio mcp lock");
        session.request(
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": arguments,
            }),
        )
    }

    fn list_resources(&self) -> Result<Vec<McpResourceDescriptor>, McpError> {
        let mut session = self.inner.lock().expect("stdio mcp lock");
        let result = session.request("resources/list", json!({}))?;
        parse_resource_list(&session.server_name, result)
    }

    fn list_prompts(&self) -> Result<Vec<McpPromptDescriptor>, McpError> {
        let mut session = self.inner.lock().expect("stdio mcp lock");
        let result = session.request("prompts/list", json!({}))?;
        parse_prompt_list(&session.server_name, result)
    }
}

impl StdioMcpSession {
    fn initialize(&mut self) -> Result<(), McpError> {
        let _ = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "deep-code",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )?;
        Ok(())
    }

    fn notify_initialized(&mut self) -> Result<(), McpError> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.read_response(id)
    }

    fn write_message(&mut self, payload: &Value) -> Result<(), McpError> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Protocol {
                server: self.server_name.clone(),
                message: "missing stdin".to_string(),
            })?;
        let encoded = serde_json::to_string(payload).map_err(|error| McpError::Protocol {
            server: self.server_name.clone(),
            message: error.to_string(),
        })?;
        stdin
            .write_all(encoded.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|error| McpError::Protocol {
                server: self.server_name.clone(),
                message: error.to_string(),
            })
    }

    fn read_response(&mut self, expected_id: u64) -> Result<Value, McpError> {
        let deadline = std::time::Instant::now() + MCP_CALL_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout {
                    server: self.server_name.clone(),
                });
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(ReaderEvent::Line(line)) => line,
                Ok(ReaderEvent::Eof) => {
                    return Err(McpError::Protocol {
                        server: self.server_name.clone(),
                        message: "unexpected EOF from MCP server".to_string(),
                    });
                }
                Ok(ReaderEvent::Error(message)) => {
                    return Err(McpError::Protocol {
                        server: self.server_name.clone(),
                        message,
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(McpError::Timeout {
                        server: self.server_name.clone(),
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Protocol {
                        server: self.server_name.clone(),
                        message: "MCP reader thread exited".to_string(),
                    });
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let message: JsonRpcMessage =
                serde_json::from_str(&line).map_err(|error| McpError::Protocol {
                    server: self.server_name.clone(),
                    message: format!("invalid json-rpc line: {error}"),
                })?;
            if message.id.is_none() {
                continue;
            }
            if message.id != Some(expected_id) {
                continue;
            }
            if let Some(error) = message.error {
                return Err(McpError::Protocol {
                    server: self.server_name.clone(),
                    message: error.message,
                });
            }
            return Ok(message.result.unwrap_or(Value::Null));
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcMessage {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcErrorBody {
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub server_name: String,
    pub tool_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceDescriptor {
    pub server_name: String,
    pub uri: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptDescriptor {
    pub server_name: String,
    pub name: String,
    pub description: Option<String>,
    pub arguments: Value,
}

pub fn connect_client(config: &McpServerConfig) -> Result<Box<dyn McpClient>, McpError> {
    config.validate()?;
    match config.transport {
        McpTransport::Stdio => Ok(Box::new(StdioMcpClient::connect(config)?)),
        McpTransport::Http => Err(McpError::UnsupportedTransport {
            server: config.name.clone(),
            transport: "http".to_string(),
        }),
    }
}

fn parse_tool_list(server_name: &str, payload: Value) -> Result<Vec<McpToolDescriptor>, McpError> {
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(McpToolDescriptor {
            server_name: server_name.to_string(),
            tool_name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_schema: tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        });
    }
    out.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
    Ok(out)
}

fn parse_resource_list(
    server_name: &str,
    payload: Value,
) -> Result<Vec<McpResourceDescriptor>, McpError> {
    let resources = payload
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for resource in resources {
        let Some(uri) = resource.get("uri").and_then(Value::as_str) else {
            continue;
        };
        out.push(McpResourceDescriptor {
            server_name: server_name.to_string(),
            uri: uri.to_string(),
            description: resource
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    out.sort_by(|left, right| left.uri.cmp(&right.uri));
    Ok(out)
}

fn parse_prompt_list(
    server_name: &str,
    payload: Value,
) -> Result<Vec<McpPromptDescriptor>, McpError> {
    let prompts = payload
        .get("prompts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for prompt in prompts {
        let Some(name) = prompt.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(McpPromptDescriptor {
            server_name: server_name.to_string(),
            name: name.to_string(),
            description: prompt
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            arguments: prompt
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!([])),
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}
