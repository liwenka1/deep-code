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
    AgentConfig, AgentEvent, AgentRuntimeHandle, ApprovalDecision, ApprovalRequest, CheckpointId,
    CheckpointStore, CostCurrency, JsonSessionStore, Message, Role, RuntimeEvent, SessionRecord,
    SessionStore, SharedSubAgentManager, TurnTelemetry, format_sessions_storage_note,
    is_subagent_tool, launch_runtime,
};
use tokio::sync::mpsc;

use crate::cli::workspace_root;

#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub resume: Option<SessionRecord>,
}

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
    pub session_id: Option<String>,
    runtime: Arc<dyn AgentRuntimeHandle>,
    backend_label: String,
    subagent_manager: SharedSubAgentManager,
    subagent_shutdown: Option<Box<dyn Fn() + Send + Sync>>,
    ui_rx: Option<UiUpdateReceiver>,
    cost_currency: CostCurrency,
}

impl App {
    #[must_use]
    pub fn launch(config: LaunchConfig) -> Self {
        let agent_config = AgentConfig::from_env();
        let cost_currency = agent_config.cost_currency;
        let workspace = workspace_root();
        let launched = launch_runtime(&agent_config, workspace, config.resume.clone());
        let runtime = launched.handle;
        let backend_label = launched.backend_label;
        let session_id = launched.session_id;
        let subagent_manager = launched.subagent_manager;
        let subagent_shutdown = Some(launched.stop_hook);
        let resumed = config.resume.is_some();
        let persistent = session_id.is_some();
        let workspace_note = std::env::current_dir()
            .map(|cwd| format_sessions_storage_note(&cwd))
            .unwrap_or_else(|_| {
                "Sessions are stored under .deep-code/sessions/ in the current workspace directory."
                    .to_string()
            });
        let mut messages = vec![ChatMessage {
            author: Author::System,
            text: format!(
                "{}\n{}\n{}\n{}\n{}",
                "Type a prompt and press Enter. Press Esc or Ctrl+C to exit.",
                "Tip: in offline mode, type \"/mock-tool hello\" to exercise approval.",
                "Slash: /checkpoints, /restore <id>, /sessions, /agents",
                workspace_note,
                if resumed {
                    "Resumed previous session."
                } else if persistent {
                    "Started a new persistent session."
                } else {
                    "Session persistence unavailable in this workspace."
                }
            ),
        }];

        if let Some(record) = config.resume.as_ref() {
            messages.extend(hydrate_messages(record));
        }

        let status = if let Some(id) = &session_id {
            format!("Ready - {backend_label} | session {id}")
        } else {
            format!("Ready - {backend_label}")
        };

        Self {
            input: String::new(),
            messages,
            streaming_buffer: String::new(),
            status,
            error: None,
            should_quit: false,
            is_streaming: false,
            pending_approval: None,
            last_checkpoint: None,
            session_id,
            runtime,
            backend_label,
            subagent_manager,
            subagent_shutdown,
            ui_rx: None,
            cost_currency,
        }
    }

    #[must_use]
    pub fn new() -> Self {
        Self::launch(LaunchConfig::default())
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
                if is_subagent_tool(&result.tool_name) {
                    self.refresh_subagent_status();
                }
            }
            RuntimeEvent::DiagnosticsUpdated { summary, rendered } => {
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!("Diagnostics: {summary}"),
                });
                if !rendered.is_empty() {
                    self.messages.push(ChatMessage {
                        author: Author::System,
                        text: rendered,
                    });
                }
                self.status = format!("Diagnostics: {summary}");
            }
            RuntimeEvent::CompactionApplied {
                archived_count,
                summary,
            } => {
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!("已压缩 {archived_count} 条历史消息\n{summary}"),
                });
                self.status = format!("已压缩 {archived_count} 条历史消息");
            }
            RuntimeEvent::TurnFinished { telemetry, .. } => {
                self.flush_assistant_buffer();
                let checkpoint = self
                    .last_checkpoint
                    .as_ref()
                    .map(|id| format!(" | 回滚: /restore {id}"))
                    .unwrap_or_default();
                let session = self
                    .session_id
                    .as_ref()
                    .map(|id| format!(" | session {id}"))
                    .unwrap_or_default();
                let telemetry_note = telemetry
                    .as_ref()
                    .map(|value| format_turn_telemetry(value, self.cost_currency))
                    .unwrap_or_default();
                self.status = format!(
                    "就绪 - {}{}{}{telemetry_note}",
                    self.backend_label, checkpoint, session
                );
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
            "/sessions" => {
                self.list_sessions();
                true
            }
            "/agents" => {
                self.list_subagents();
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

    fn list_subagents(&mut self) {
        let manager = match self.subagent_manager.read() {
            Ok(manager) => manager,
            Err(error) => {
                self.status = format!("Sub-agents unavailable: {error}");
                return;
            }
        };
        let agents = manager.list_current_session();
        if agents.is_empty() {
            self.status = "No sub-agents in this session.".to_string();
            return;
        }
        let body = agents
            .iter()
            .map(|agent| {
                let handle = agent
                    .transcript_handle
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("-");
                format!(
                    "- {} [{}] {} | handle={handle} | {}",
                    agent.name,
                    agent.status.as_str(),
                    agent.role,
                    agent.short_summary()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.messages.push(ChatMessage {
            author: Author::System,
            text: format!("Sub-agents:\n{body}"),
        });
        self.status = format!(
            "{} sub-agent(s), {} running",
            agents.len(),
            manager.running_count()
        );
    }

    fn refresh_subagent_status(&mut self) {
        if let Ok(manager) = self.subagent_manager.read() {
            let running = manager.running_count();
            if running > 0 {
                self.status = format!(
                    "Ready - {} | {} sub-agent(s) running",
                    self.backend_label, running
                );
            }
        }
    }

    fn list_sessions(&mut self) {
        let Ok(cwd) = std::env::current_dir() else {
            self.status = "Cannot resolve workspace for sessions.".to_string();
            return;
        };
        match JsonSessionStore::for_workspace(cwd) {
            Ok(store) => match store.list() {
                Ok(records) if records.is_empty() => {
                    self.status = "No saved sessions.".to_string();
                }
                Ok(records) => {
                    let note = std::env::current_dir()
                        .map(|cwd| format_sessions_storage_note(&cwd))
                        .unwrap_or_default();
                    self.messages.push(ChatMessage {
                        author: Author::System,
                        text: format!(
                            "{note}\nSessions:\n{}",
                            records
                                .iter()
                                .map(|record| {
                                    format!(
                                        "- {} ({} msgs) {}",
                                        record.id.as_str(),
                                        record.messages.len(),
                                        record.preview().replace('\n', " ")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                    });
                    self.status =
                        format!("{} session(s). CLI: deep-code session list", records.len());
                }
                Err(error) => self.status = format!("List failed: {error}"),
            },
            Err(error) => self.status = format!("Sessions unavailable: {error}"),
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
                    self.apply_runtime_event(RuntimeEvent::WorkspaceRestored { id: checkpoint_id });
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

    pub async fn shutdown_runtime(&self) {
        if let Some(shutdown) = &self.subagent_shutdown {
            shutdown();
        }
        self.runtime.shutdown().await;
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

fn hydrate_messages(record: &SessionRecord) -> Vec<ChatMessage> {
    record.messages.iter().filter_map(message_to_chat).collect()
}

fn message_to_chat(message: &Message) -> Option<ChatMessage> {
    match message.role {
        Role::System => None,
        Role::User => Some(ChatMessage {
            author: Author::User,
            text: message.content.clone(),
        }),
        Role::Assistant => {
            let mut text = message.content.clone();
            if !message.tool_calls.is_empty() {
                let tools = message
                    .tool_calls
                    .iter()
                    .map(|call| call.function.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if text.is_empty() {
                    text = format!("(requested tools: {tools})");
                } else {
                    text = format!("{text}\n(requested tools: {tools})");
                }
            }
            Some(ChatMessage {
                author: Author::Assistant,
                text,
            })
        }
        Role::Tool => Some(ChatMessage {
            author: Author::System,
            text: format!(
                "Tool result ({}): {}",
                message.tool_call_id.as_deref().unwrap_or("unknown"),
                summarize_tool_result(&message.content)
            ),
        }),
    }
}

fn summarize_tool_result(content: &str) -> String {
    const MAX_CHARS: usize = 300;

    if content.contains("<diagnostics file=")
        && let Some(block_start) = content.find("<diagnostics file=")
    {
        let prefix = content[..block_start].trim();
        let diagnostics = &content[block_start..];
        let diag_summary = diagnostics
            .lines()
            .find(|line| line.starts_with("  ERROR") || line.starts_with("  WARNING"))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| "diagnostics attached".to_string());
        if prefix.is_empty() {
            return truncate_chars(&diag_summary, MAX_CHARS);
        }
        return truncate_chars(&format!("{prefix} | {diag_summary}"), MAX_CHARS);
    }

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

fn format_turn_telemetry(telemetry: &TurnTelemetry, currency: CostCurrency) -> String {
    let turn = telemetry.turn_cost.format(currency);
    let session = telemetry.session_cost.format(currency);
    let cache = match (telemetry.cache_hit_tokens, telemetry.cache_miss_tokens) {
        (Some(hit), Some(miss)) => format!(" | cache {hit}/{miss}"),
        _ => String::new(),
    };
    let context = format!(
        "ctx {}/{} ({}%)",
        telemetry.estimated_context_tokens,
        telemetry.context_window,
        telemetry.context_usage_percent
    );
    let compaction = if telemetry.near_compaction_threshold {
        " | 接近压缩阈值"
    } else {
        ""
    };
    format!(
        " | {} | 本回合 {} | 累计 {}{cache} | {context}{compaction} | {}",
        telemetry.route_label,
        turn,
        session,
        telemetry.prefix_status.label_zh()
    )
}
