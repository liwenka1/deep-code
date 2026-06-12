use std::collections::{HashMap, VecDeque};

use deep_code_agent::{Message, Role, SessionRecord, ToolResultStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalState {
    NotRequired,
    Required,
    Approved,
    Denied,
}

impl ToolApprovalState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRequired => "not required",
            Self::Required => "required",
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        tool_name: String,
        arguments: String,
        risk_level: Option<String>,
        requires_sandbox: Option<bool>,
        approval: ToolApprovalState,
    },
    ToolResult {
        tool_name: String,
        status: ToolResultStatus,
        summary: String,
    },
    Approval {
        tool_name: String,
        description: String,
        risk_level: String,
        requires_sandbox: bool,
        matched_rule: Option<String>,
        arguments: String,
    },
    Diagnostics {
        summary: String,
        rendered: String,
    },
    Checkpoint {
        id: String,
        label: String,
    },
    Compaction {
        metadata: Option<String>,
        summary: String,
    },
}

impl HistoryCell {
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self::System { text: text.into() }
    }

    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant { text: text.into() }
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::System { .. } => "System",
            Self::User { .. } => "You",
            Self::Assistant { .. } => "Assistant",
            Self::Reasoning { .. } => "Reasoning",
            Self::ToolCall { .. } => "Tool call",
            Self::ToolResult { .. } => "Tool result",
            Self::Approval { .. } => "Approval",
            Self::Diagnostics { .. } => "Diagnostics",
            Self::Checkpoint { .. } => "Checkpoint",
            Self::Compaction { .. } => "Compaction",
        }
    }

    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::System { text }
            | Self::User { text }
            | Self::Assistant { text }
            | Self::Reasoning { text } => vec![text.clone()],
            Self::ToolCall {
                tool_name,
                arguments,
                risk_level,
                requires_sandbox,
                approval,
            } => vec![
                format!("Tool: {tool_name}"),
                format!("Approval: {}", approval.label()),
                format!(
                    "Risk: {} | Sandbox: {}",
                    risk_level.as_deref().unwrap_or("unknown"),
                    requires_sandbox
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ),
                format!("Arguments: {}", truncate_chars(arguments, 240)),
            ],
            Self::ToolResult {
                tool_name,
                status,
                summary,
            } => vec![
                format!("Tool: {tool_name}"),
                format!("Result: {status:?}"),
                format!("Summary: {}", truncate_chars(summary, 300)),
            ],
            Self::Approval {
                tool_name,
                description,
                risk_level,
                requires_sandbox,
                matched_rule,
                arguments,
            } => vec![
                format!("Tool: {tool_name}"),
                format!("Risk: {risk_level} | Sandbox: {requires_sandbox}"),
                format!("Rule: {}", matched_rule.as_deref().unwrap_or("none")),
                format!("Description: {}", truncate_chars(description, 200)),
                format!("Arguments: {}", truncate_chars(arguments, 240)),
                "Press y to approve, n to deny.".to_string(),
            ],
            Self::Diagnostics { summary, rendered } => {
                if rendered.is_empty() {
                    vec![summary.clone()]
                } else {
                    vec![summary.clone(), truncate_chars(rendered, 600)]
                }
            }
            Self::Checkpoint { id, label } => vec![
                format!("Label: {label}"),
                format!("ID: {id}"),
                format!("Restore: /restore {id}"),
            ],
            Self::Compaction { metadata, summary } => {
                let title = metadata
                    .as_deref()
                    .map(|value| format!("Compaction summary ({value})"))
                    .unwrap_or_else(|| "Compaction summary".to_string());
                vec![title, summary.clone()]
            }
        }
    }
}

pub(crate) fn hydrate_history(record: &SessionRecord) -> Vec<HistoryCell> {
    let mut tool_names = HashMap::new();
    let mut tool_results: HashMap<String, VecDeque<(String, ToolResultStatus)>> = HashMap::new();
    for result in record.turns.iter().flat_map(|turn| &turn.tool_results) {
        tool_results
            .entry(result.call_id.clone())
            .or_default()
            .push_back((result.tool_name.clone(), result.status.clone()));
    }

    let mut cells = Vec::new();
    let mut turn_index = 0usize;
    let mut current_turn = Vec::new();

    for message in &record.messages {
        match message.role {
            Role::User => {
                if !current_turn.is_empty() {
                    cells.append(&mut current_turn);
                    append_turn_checkpoints(&mut cells, record, turn_index);
                    turn_index += 1;
                }
                current_turn.push(HistoryCell::user(message.content.clone()));
            }
            Role::System => {}
            _ => {
                current_turn.extend(message_to_cells(
                    message,
                    &mut tool_names,
                    &mut tool_results,
                ));
            }
        }
    }
    if !current_turn.is_empty() {
        cells.extend(current_turn);
        append_turn_checkpoints(&mut cells, record, turn_index);
    }

    if let Some(summary) = &record.summary {
        cells.push(HistoryCell::Compaction {
            metadata: record.compaction.clone(),
            summary: summary.clone(),
        });
    }

    cells
}

fn append_turn_checkpoints(
    cells: &mut Vec<HistoryCell>,
    record: &SessionRecord,
    turn_index: usize,
) {
    let Some(turn) = record.turns.get(turn_index) else {
        return;
    };
    let window_end = record
        .turns
        .get(turn_index + 1)
        .map_or(u64::MAX, |next| next.started_at_ms);
    for checkpoint in &record.checkpoints {
        if checkpoint.created_at_ms >= turn.started_at_ms
            && checkpoint.created_at_ms < window_end
        {
            cells.push(HistoryCell::Checkpoint {
                id: checkpoint.id.0.clone(),
                label: checkpoint.label.clone(),
            });
        }
    }
}

fn message_to_cells(
    message: &Message,
    tool_names: &mut HashMap<String, String>,
    tool_results: &mut HashMap<String, VecDeque<(String, ToolResultStatus)>>,
) -> Vec<HistoryCell> {
    match message.role {
        Role::System => Vec::new(),
        Role::User => vec![HistoryCell::user(message.content.clone())],
        Role::Assistant => {
            let mut cells = Vec::new();
            if let Some(reasoning) = message
                .reasoning_content
                .as_ref()
                .filter(|text| !text.is_empty())
            {
                cells.push(HistoryCell::Reasoning {
                    text: reasoning.clone(),
                });
            }
            if !message.content.is_empty() {
                cells.push(HistoryCell::assistant(message.content.clone()));
            }
            cells.extend(message.tool_calls.iter().map(|call| {
                tool_names.insert(call.id.clone(), call.function.name.clone());
                HistoryCell::ToolCall {
                    tool_name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                    risk_level: None,
                    requires_sandbox: None,
                    approval: ToolApprovalState::NotRequired,
                }
            }));
            cells
        }
        Role::Tool => {
            let call_id = message.tool_call_id.as_deref().unwrap_or("unknown");
            let result = tool_results.get_mut(call_id).and_then(VecDeque::pop_front);
            vec![HistoryCell::ToolResult {
                tool_name: result
                    .as_ref()
                    .map(|(tool_name, _)| tool_name.clone())
                    .or_else(|| tool_names.get(call_id).cloned())
                    .unwrap_or_else(|| call_id.to_string()),
                status: result
                    .map(|(_, status)| status)
                    .unwrap_or(ToolResultStatus::Success),
                summary: summarize_tool_result(&message.content),
            }]
        }
    }
}
pub(crate) fn summarize_tool_result(content: &str) -> String {
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

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut truncated = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() {
        truncated.push_str("... (truncated)");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use deep_code_agent::{AgentConfig, Message, SessionRecord, ToolCallPayload};

    use super::*;

    #[test]
    fn hydrate_history_keeps_assistant_tool_calls_and_results() {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "");
        record.messages.push(Message::user("hi"));
        record.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCallPayload {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: deep_code_agent::ToolCallFunctionPayload {
                    name: "mock_echo".to_string(),
                    arguments: "{\"message\":\"hi\"}".to_string(),
                },
            }],
        ));
        record
            .messages
            .push(Message::tool("call_1", "mock_echo: hi"));
        let mut turn = deep_code_agent::TurnRecord::new("hi");
        turn.tool_results.push(deep_code_agent::ToolResult {
            call_id: "call_1".to_string(),
            tool_name: "mock_echo".to_string(),
            status: ToolResultStatus::Denied,
            content: "mock_echo: hi".to_string(),
        });
        turn.started_at_ms = 10;
        turn.finished_at_ms = Some(20);
        record.turns.push(turn);
        record.summary = Some("older conversation summary".to_string());
        record.compaction = Some("archived=2".to_string());
        let mut checkpoint = deep_code_agent::CheckpointRecord::new(
            deep_code_agent::CheckpointId("checkpoint_1".to_string()),
            "before_turn",
        );
        checkpoint.created_at_ms = 15;
        record.checkpoints.push(checkpoint);

        let cells = hydrate_history(&record);
        assert!(matches!(cells[0], HistoryCell::User { .. }));
        assert!(matches!(
            &cells[1],
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
        ));
        assert!(matches!(
            &cells[2],
            HistoryCell::ToolResult {
                tool_name,
                status,
                summary,
            } if tool_name == "mock_echo"
                && *status == ToolResultStatus::Denied
                && summary.contains("mock_echo")
        ));
        assert!(cells.iter().any(|cell| matches!(
            cell,
            HistoryCell::Compaction { metadata, summary }
                if metadata.as_deref() == Some("archived=2")
                    && summary == "older conversation summary"
        )));
        assert!(cells.iter().any(|cell| matches!(
            cell,
            HistoryCell::Checkpoint { id, .. } if id == "checkpoint_1"
        )));
    }

    #[test]
    fn hydrate_history_restores_reasoning_content() {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "");
        record.messages.push(Message::user("hi"));
        record.messages.push(Message::assistant_turn(
            "answer",
            "thinking",
            Vec::new(),
        ));

        let cells = hydrate_history(&record);
        assert!(matches!(
            &cells[1],
            HistoryCell::Reasoning { text } if text == "thinking"
        ));
        assert!(matches!(
            &cells[2],
            HistoryCell::Assistant { text } if text == "answer"
        ));
    }

    #[test]
    fn hydrate_history_matches_duplicate_tool_call_ids_in_order() {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "");
        record.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCallPayload {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: deep_code_agent::ToolCallFunctionPayload {
                    name: "first_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        record
            .messages
            .push(Message::tool("call_1", "first result"));
        record.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCallPayload {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: deep_code_agent::ToolCallFunctionPayload {
                    name: "second_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        record
            .messages
            .push(Message::tool("call_1", "second result"));

        let mut first_turn = deep_code_agent::TurnRecord::new("first");
        first_turn.tool_results.push(deep_code_agent::ToolResult {
            call_id: "call_1".to_string(),
            tool_name: "first_tool".to_string(),
            status: ToolResultStatus::Denied,
            content: "first result".to_string(),
        });
        let mut second_turn = deep_code_agent::TurnRecord::new("second");
        second_turn.tool_results.push(deep_code_agent::ToolResult {
            call_id: "call_1".to_string(),
            tool_name: "second_tool".to_string(),
            status: ToolResultStatus::Error,
            content: "second result".to_string(),
        });
        record.turns.push(first_turn);
        record.turns.push(second_turn);

        let tool_results = hydrate_history(&record)
            .into_iter()
            .filter_map(|cell| match cell {
                HistoryCell::ToolResult {
                    tool_name, status, ..
                } => Some((tool_name, status)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            tool_results,
            vec![
                ("first_tool".to_string(), ToolResultStatus::Denied),
                ("second_tool".to_string(), ToolResultStatus::Error),
            ]
        );
    }

    #[test]
    fn tool_and_approval_lines_truncate_long_fields() {
        let long = "x".repeat(500);
        let tool = HistoryCell::ToolCall {
            tool_name: "write_file".to_string(),
            arguments: long.clone(),
            risk_level: None,
            requires_sandbox: None,
            approval: ToolApprovalState::NotRequired,
        };
        assert!(tool.lines().iter().any(|line| line.contains("(truncated)")));

        let approval = HistoryCell::Approval {
            tool_name: "shell_run".to_string(),
            description: long.clone(),
            risk_level: "High".to_string(),
            requires_sandbox: true,
            matched_rule: Some("shell".to_string()),
            arguments: long,
        };
        let lines = approval.lines();
        assert!(lines.iter().any(|line| line.contains("Risk: High")));
        assert!(lines.iter().any(|line| line.contains("Rule: shell")));
        assert!(lines.iter().any(|line| line.contains("(truncated)")));
    }

    #[test]
    fn checkpoint_lines_include_restore_command() {
        let cell = HistoryCell::Checkpoint {
            id: "checkpoint_1".to_string(),
            label: "before_turn".to_string(),
        };
        assert!(
            cell.lines()
                .iter()
                .any(|line| line == "Restore: /restore checkpoint_1")
        );
    }
}
