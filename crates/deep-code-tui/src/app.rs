//! TUI application state.
//!
//! This module is intentionally thin: the agent runtime owns the model loop,
//! tool registry, session, and approval gating. The UI only has to:
//!
//! 1. forward user prompts via [`AgentRuntimeHandle::submit_user`],
//! 2. render [`RuntimeEvent`]s as they arrive,
//! 3. forward approval decisions via [`AgentRuntimeHandle::submit_approval`].

use std::sync::Arc;

use deep_code_agent::{
    AgentConfig, AgentEvent, AgentRuntime, AgentRuntimeHandle, ApprovalDecision, ApprovalRequest,
    CheckpointId, CheckpointStore, DeepSeekClient, RuntimeEvent, ToolRegistry, git_tool_registry,
    shell_tool_registry, workspace_tool_registry,
};
use tokio::sync::mpsc;

use crate::echo_client::EchoClient;

const SYSTEM_PROMPT: &str = "You are deep-code's minimal TUI assistant.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub author: Author,
    pub text: String,
}

/// Updates pushed from the bridge task into the UI thread.
#[derive(Debug, Clone, PartialEq)]
enum UiUpdate {
    Event(RuntimeEvent),
    StreamFinished,
}

type UiUpdateReceiver = mpsc::UnboundedReceiver<UiUpdate>;

pub struct App {
    pub input: String,
    pub messages: Vec<ChatMessage>,
    pub streaming_buffer: String,
    pub status: String,
    pub error: Option<String>,
    pub should_quit: bool,
    pub is_streaming: bool,
    pub pending_approval: Option<ApprovalRequest>,
    pub last_checkpoint: Option<String>,
    runtime: Arc<dyn AgentRuntimeHandle>,
    backend_label: String,
    ui_rx: Option<UiUpdateReceiver>,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        let config = AgentConfig::from_env();
        let (runtime, backend_label) = build_runtime(&config);
        let status = format!("Ready - {backend_label}");

        Self {
            input: String::new(),
            messages: vec![ChatMessage {
                author: Author::System,
                text: format!(
                    "{}\n{}\n{}",
                    "Type a prompt and press Enter. Press Esc or Ctrl+C to exit.",
                    "Tip: in offline mode, type \"/mock-tool hello\" to exercise approval.",
                    "Slash: /checkpoints, /restore <id>"
                ),
            }],
            streaming_buffer: String::new(),
            status,
            error: None,
            should_quit: false,
            is_streaming: false,
            pending_approval: None,
            last_checkpoint: None,
            runtime,
            backend_label,
            ui_rx: None,
        }
    }

    pub fn push_char(&mut self, value: char) {
        if !self.is_streaming {
            self.input.push(value);
        }
    }

    pub fn backspace(&mut self) {
        if !self.is_streaming {
            self.input.pop();
        }
    }

    pub fn submit(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            return;
        }

        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            self.status = "Enter a prompt before sending.".to_string();
            return;
        }

        if prompt.starts_with('/') && self.handle_slash_command(&prompt) {
            self.input.clear();
            return;
        }

        self.input.clear();
        self.error = None;
        self.streaming_buffer.clear();
        self.is_streaming = true;
        self.status = format!("Streaming from {}...", self.backend_label);

        self.messages.push(ChatMessage {
            author: Author::User,
            text: prompt.clone(),
        });

        self.start_stream(StreamRequest::User(prompt));
    }

    pub fn approve_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Approved);
    }

    pub fn deny_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Denied);
    }

    pub fn drain_stream_updates(&mut self) {
        let Some(mut rx) = self.ui_rx.take() else {
            return;
        };

        while let Ok(update) = rx.try_recv() {
            self.apply_ui_update(update);
        }

        if self.is_streaming {
            self.ui_rx = Some(rx);
        }
    }

    fn resolve_pending_tool(&mut self, decision: ApprovalDecision) {
        if self.pending_approval.take().is_none() {
            return;
        }

        let label = match decision {
            ApprovalDecision::Approved => "approved",
            ApprovalDecision::Denied => "denied",
        };
        self.status = format!("Tool {label}, resuming...");
        self.is_streaming = true;
        self.start_stream(StreamRequest::Approval(decision));
    }

    fn start_stream(&mut self, request: StreamRequest) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);

        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let mut events = match request {
                StreamRequest::User(prompt) => runtime.submit_user(prompt).await,
                StreamRequest::Approval(decision) => runtime.submit_approval(decision).await,
            };

            while let Some(event) = events.recv().await {
                if tx.send(UiUpdate::Event(event.clone())).is_err() {
                    return;
                }
                if matches!(
                    event,
                    RuntimeEvent::TurnFinished { .. }
                        | RuntimeEvent::ApprovalRequired { .. }
                        | RuntimeEvent::Error { .. }
                ) {
                    break;
                }
            }

            let _ = tx.send(UiUpdate::StreamFinished);
        });
    }

    fn apply_ui_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Event(event) => self.apply_runtime_event(event),
            UiUpdate::StreamFinished => {
                self.is_streaming = false;
                self.ui_rx = None;
            }
        }
    }

    fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Provider(AgentEvent::TextDelta { text })
            | RuntimeEvent::Provider(AgentEvent::ReasoningDelta { text }) => {
                self.streaming_buffer.push_str(&text);
            }
            RuntimeEvent::Provider(AgentEvent::ToolCallDelta { .. }) => {
                self.status = "Receiving tool call...".to_string();
            }
            RuntimeEvent::Provider(AgentEvent::Done { .. }) => {
                // Provider stream finished; runtime will send TurnFinished or
                // ApprovalRequired next. Nothing to do here.
            }
            RuntimeEvent::Provider(AgentEvent::Error { message }) => {
                self.record_error(message);
            }
            RuntimeEvent::ApprovalRequired { request } => {
                self.flush_assistant_buffer();
                let sandbox = if request.requires_sandbox {
                    "yes"
                } else {
                    "no"
                };
                self.status = format!(
                    "Approve '{}' (risk={:?}, sandbox={sandbox})? y/n",
                    request.tool_name, request.risk_level
                );
                self.pending_approval = Some(request);
                self.is_streaming = false;
                self.ui_rx = None;
            }
            RuntimeEvent::CheckpointCreated { id, label } => {
                self.last_checkpoint = Some(id.0.clone());
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!("Checkpoint [{label}]: {}", id.0),
                });
            }
            RuntimeEvent::WorkspaceRestored { id } => {
                self.last_checkpoint = Some(id.0.clone());
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!("Workspace restored from checkpoint {}", id.0),
                });
                self.status = format!("Restored checkpoint {}", id.0);
            }
            RuntimeEvent::ToolResult { result } => {
                let summary = summarize_tool_result(&result.content);
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!(
                        "Tool result ({} / {:?}): {}",
                        result.tool_name, result.status, summary
                    ),
                });
            }
            RuntimeEvent::TurnFinished { .. } => {
                self.flush_assistant_buffer();
                let checkpoint = self
                    .last_checkpoint
                    .as_ref()
                    .map(|id| format!(" | rollback: /restore {id}"))
                    .unwrap_or_default();
                self.status = format!("Ready - {}{checkpoint}", self.backend_label);
                self.is_streaming = false;
                self.ui_rx = None;
            }
            RuntimeEvent::Error { message } => self.record_error(message),
        }
    }

    fn flush_assistant_buffer(&mut self) {
        if self.streaming_buffer.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.streaming_buffer);
        self.messages.push(ChatMessage {
            author: Author::Assistant,
            text,
        });
    }

    fn handle_slash_command(&mut self, prompt: &str) -> bool {
        match prompt {
            "/checkpoints" => {
                self.list_checkpoints();
                true
            }
            _ if prompt.starts_with("/restore ") => {
                let id = prompt.trim_start_matches("/restore ").trim();
                if id.is_empty() {
                    self.status = "Usage: /restore <checkpoint_id>".to_string();
                } else {
                    self.restore_checkpoint(id);
                }
                true
            }
            _ => false,
        }
    }

    fn list_checkpoints(&mut self) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for checkpoints.".to_string();
            return;
        };
        match CheckpointStore::new(cwd) {
            Ok(store) => match store.list() {
                Ok(ids) if ids.is_empty() => {
                    self.status = "No checkpoints yet.".to_string();
                }
                Ok(ids) => {
                    self.messages.push(ChatMessage {
                        author: Author::System,
                        text: format!(
                            "Checkpoints:\n{}",
                            ids.iter()
                                .map(|id| format!("- {}", id.0))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                    });
                    self.status = format!("{} checkpoint(s).", ids.len());
                }
                Err(error) => self.status = format!("List failed: {error}"),
            },
            Err(error) => self.status = format!("Checkpoints unavailable: {error}"),
        }
    }

    fn restore_checkpoint(&mut self, id: &str) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for restore.".to_string();
            return;
        };
        let checkpoint_id = CheckpointId(id.to_string());
        match CheckpointStore::new(cwd) {
            Ok(store) => match store.restore(&checkpoint_id) {
                Ok(()) => {
                    self.last_checkpoint = Some(id.to_string());
                    self.apply_runtime_event(RuntimeEvent::WorkspaceRestored {
                        id: checkpoint_id,
                    });
                }
                Err(error) => self.status = format!("Restore failed: {error}"),
            },
            Err(error) => self.status = format!("Checkpoints unavailable: {error}"),
        }
    }

    fn record_error(&mut self, message: String) {
        self.error = Some(message.clone());
        self.status = "Agent error.".to_string();
        self.messages.push(ChatMessage {
            author: Author::System,
            text: format!("Error: {message}"),
        });
        self.is_streaming = false;
        self.ui_rx = None;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

enum StreamRequest {
    User(String),
    Approval(ApprovalDecision),
}

fn build_runtime(config: &AgentConfig) -> (Arc<dyn AgentRuntimeHandle>, String) {
    let tool_registry = default_tool_registry();
    if config.api_key.is_some() {
        match DeepSeekClient::new(config.clone()) {
            Ok(client) => {
                let runtime = attach_checkpoints(AgentRuntime::with_system_prompt(
                    client,
                    tool_registry,
                    SYSTEM_PROMPT,
                ));
                let label = format!("DeepSeek {}", config.model);
                return (Arc::new(runtime) as Arc<dyn AgentRuntimeHandle>, label);
            }
            Err(_) => {
                // Fall through to offline echo. The runtime is still useful
                // for trying out the UX without a key.
            }
        }
    }

    let runtime = attach_checkpoints(AgentRuntime::with_system_prompt(
        EchoClient,
        tool_registry,
        SYSTEM_PROMPT,
    ));
    let label = "offline echo (set DEEPSEEK_API_KEY for DeepSeek)".to_string();
    (Arc::new(runtime) as Arc<dyn AgentRuntimeHandle>, label)
}

fn attach_checkpoints<C: deep_code_agent::LlmClient + 'static>(
    runtime: AgentRuntime<C>,
) -> AgentRuntime<C> {
    match std::env::current_dir() {
        Ok(cwd) => runtime.with_checkpoints(cwd),
        Err(error) => {
            eprintln!("checkpoints disabled: {error}");
            runtime
        }
    }
}

fn summarize_tool_result(content: &str) -> String {
    const MAX_CHARS: usize = 300;

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(summary) = summarize_json_tool_result(&value)
    {
        return summary;
    }

    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flattened, MAX_CHARS)
}

fn summarize_json_tool_result(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");

    if let Some(entries) = object.get("entries").and_then(serde_json::Value::as_array) {
        return Some(format!("{path}: {} entries", entries.len()));
    }

    if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_array) {
        let total_lines = object
            .get("total_lines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(lines.len() as u64);
        let truncated = object
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        return Some(format!(
            "{path}: {} lines shown of {total_lines} (truncated={truncated})",
            lines.len()
        ));
    }

    if let Some(matches) = object.get("matches").and_then(serde_json::Value::as_array) {
        let files_searched = object
            .get("files_searched")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let truncated = object
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        return Some(format!(
            "{path}: {} matches across {files_searched} files (truncated={truncated})",
            matches.len()
        ));
    }

    if let Some(bytes_written) = object
        .get("bytes_written")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(format!("{path}: wrote {bytes_written} bytes"));
    }

    if let Some(replacements) = object
        .get("replacements")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(format!("{path}: {replacements} replacements"));
    }

    if let Some(command) = object.get("command").and_then(serde_json::Value::as_str) {
        let status = object
            .get("status")
            .or_else(|| object.get("tool_status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let cwd = object
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        if let Some(job_id) = object.get("job_id").and_then(serde_json::Value::as_str) {
            return Some(format!("{job_id}: {status} in {cwd} ({command})"));
        }
        if object.contains_key("stdout") || object.contains_key("stderr") {
            let exit = object
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map_or("none".to_string(), |code| code.to_string());
            return Some(format!("{status} exit={exit} in {cwd} ({command})"));
        }
        if object.contains_key("diff") {
            let truncated = object
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return Some(format!("git diff in {cwd} (truncated={truncated})"));
        }
        if object.contains_key("status_output") {
            let entries = object
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .map_or(0, |entries| entries.len());
            return Some(format!("git status in {cwd}: {entries} entries"));
        }
        if object.contains_key("log") {
            let lines = object
                .get("log")
                .and_then(serde_json::Value::as_str)
                .map(|value| value.lines().count())
                .unwrap_or(0);
            return Some(format!("git log in {cwd}: {lines} lines"));
        }
    }

    if let Some(job_id) = object.get("job_id").and_then(serde_json::Value::as_str) {
        let status = object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("{job_id}: {status}"));
    }

    None
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut truncated = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() {
        truncated.push_str("...");
    }
    truncated
}

fn default_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::with_mock_tools();
    match std::env::current_dir() {
        Ok(cwd) => {
            match workspace_tool_registry(cwd.clone()) {
                Ok(workspace_tools) => registry.extend(workspace_tools),
                Err(error) => eprintln!("workspace tools disabled: {error}"),
            }
            match shell_tool_registry(cwd.clone()) {
                Ok(shell_tools) => registry.extend(shell_tools),
                Err(error) => eprintln!("shell tools disabled: {error}"),
            }
            match git_tool_registry(cwd) {
                Ok(git_tools) => registry.extend(git_tools),
                Err(error) => eprintln!("git tools disabled: {error}"),
            }
        }
        Err(error) => eprintln!("workspace tools disabled: {error}"),
    }
    registry
}
