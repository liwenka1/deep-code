use deep_code_agent::{EntryKind, SessionRecord, ToolResultStatus};

use crate::i18n::{Lang, TextId, tr, tr_with};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalState {
    NotRequired,
    Required,
    Approved,
    Denied,
}

impl ToolApprovalState {
    #[must_use]
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::NotRequired => "",
            Self::Required => tr(lang, TextId::BadgeRequired),
            Self::Approved => tr(lang, TextId::BadgeApproved),
            Self::Denied => tr(lang, TextId::BadgeDenied),
        }
    }
}

/// Welcome 卡的会话概要行:恢复(带轮数)/新会话(是否持久化)。
/// 供 `ui::cell_lines` 的 Welcome 渲染使用。
#[must_use]
pub(crate) fn session_summary(
    lang: Lang,
    resumed_turns: Option<usize>,
    persistent: bool,
) -> String {
    match resumed_turns {
        Some(turns) => tr_with(
            lang,
            TextId::SessionResumed,
            &[("turns", &turns.to_string())],
        ),
        None if persistent => tr(lang, TextId::SessionNewPersistent).to_string(),
        None => tr(lang, TextId::SessionNewEphemeral).to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    /// The startup header: a compact, styled welcome card (rendered specially
    /// in `cell_lines`). It scrolls away naturally after the first message.
    /// Holds raw data, not preformatted text, so a `/lang` switch re-renders
    /// it in the new language on the next frame.
    Welcome {
        version: String,
        /// Raw model id, e.g. "deepseek-chat".
        model: String,
        /// Raw reasoning setting, e.g. "medium".
        reasoning: String,
        offline: bool,
        /// Home-relative workspace path, left-truncated at render time.
        workspace: String,
        /// `Some(turns)` when resumed; `None` for a fresh session.
        resumed_turns: Option<usize>,
        /// Whether the fresh session is persisted (ignored when resumed).
        persistent: bool,
    },
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
    /// Live output tail of a still-running tool (streaming shell). Preview
    /// only: it is never flushed into the persistent transcript — the final
    /// ToolResult summary replaces it.
    ToolStream {
        text: String,
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
    pub fn lines(&self, lang: Lang) -> Vec<String> {
        match self {
            // Welcome renders exclusively through `ui::cell_lines` (it needs
            // the styled header/rule/intro), so its plain-text form is never
            // requested — no duplicate formatting kept here.
            Self::Welcome { .. } => Vec::new(),
            Self::System { text }
            | Self::User { text }
            | Self::Assistant { text }
            | Self::Reasoning { text } => vec![text.clone()],
            // Compact single line: detailed risk/sandbox/rule live in the
            // approval panel; here we only show name + args, plus an approval
            // badge when the call was actually gated.
            Self::ToolCall {
                tool_name,
                arguments,
                approval,
                ..
            } => {
                let args = truncate_chars(&collapse_whitespace(arguments), 72);
                let badge = match approval {
                    ToolApprovalState::NotRequired => String::new(),
                    other => format!(" [{}]", other.label(lang)),
                };
                vec![format!("{tool_name}  {args}{badge}")]
            }
            Self::ToolResult {
                status, summary, ..
            } => {
                vec![format!(
                    "{} {}",
                    tool_result_word(status),
                    truncate_chars(&collapse_whitespace(summary), 88)
                )]
            }
            Self::ToolStream { text } => text.lines().map(str::to_string).collect(),
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
                tr_with(lang, TextId::CheckpointLabel, &[("label", label)]),
                format!("ID: {id}"),
                tr_with(lang, TextId::CheckpointRestoreHint, &[("id", id)]),
            ],
            Self::Compaction { metadata, summary } => {
                let title = metadata.as_deref().map_or_else(
                    || tr(lang, TextId::CompactionSummaryTitle).to_string(),
                    |value| tr_with(lang, TextId::CompactionSummaryTitleMeta, &[("meta", value)]),
                );
                vec![title, summary.clone()]
            }
        }
    }
}

pub(crate) fn hydrate_history(record: &SessionRecord) -> Vec<HistoryCell> {
    let mut cells = Vec::new();
    let mut turn_index = 0usize;
    let mut current_turn = Vec::new();

    for entry in &record.entries {
        match &entry.kind {
            EntryKind::User { content } => {
                if !current_turn.is_empty() {
                    cells.append(&mut current_turn);
                    append_turn_checkpoints(&mut cells, record, turn_index);
                    turn_index += 1;
                }
                current_turn.push(HistoryCell::user(content.clone()));
            }
            EntryKind::System { .. } => {}
            EntryKind::Assistant {
                content,
                reasoning,
                exchanges,
            } => {
                if let Some(reasoning) = reasoning.as_ref().filter(|text| !text.is_empty()) {
                    current_turn.push(HistoryCell::Reasoning {
                        text: reasoning.clone(),
                    });
                }
                if !content.is_empty() {
                    current_turn.push(HistoryCell::assistant(content.clone()));
                }
                for exchange in exchanges {
                    current_turn.push(HistoryCell::ToolCall {
                        tool_name: exchange.call.function.name.clone(),
                        arguments: exchange.call.function.arguments.clone(),
                        risk_level: None,
                        requires_sandbox: None,
                        approval: ToolApprovalState::NotRequired,
                    });
                    // Pending exchanges (interrupted before a result) render
                    // the call only — no fabricated result line.
                    if let Some(result) = &exchange.result {
                        current_turn.push(HistoryCell::ToolResult {
                            tool_name: exchange.call.function.name.clone(),
                            status: result.status,
                            summary: summarize_tool_result(&result.content),
                        });
                    }
                }
            }
            EntryKind::Compaction {
                summary,
                archived_count,
            } => {
                current_turn.push(HistoryCell::Compaction {
                    metadata: Some(format!("archived={archived_count}")),
                    summary: summary.clone(),
                });
            }
        }
    }
    if !current_turn.is_empty() {
        cells.extend(current_turn);
        append_turn_checkpoints(&mut cells, record, turn_index);
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
        if checkpoint.created_at_ms >= turn.started_at_ms && checkpoint.created_at_ms < window_end {
            cells.push(HistoryCell::Checkpoint {
                id: checkpoint.id.0.clone(),
                label: checkpoint.label.clone(),
            });
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

/// Collapse all runs of whitespace (incl. newlines) to single spaces so a
/// multi-line JSON argument or tool output renders on one line.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[must_use]
pub(crate) fn tool_result_word(status: &ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Success => "✓",
        ToolResultStatus::Error => "✗",
        ToolResultStatus::Denied => "⊘",
    }
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

    use deep_code_agent::{
        AgentConfig, ExchangeResult, SessionEntry, SessionRecord, ToolCallPayload, ToolExchange,
    };

    use super::*;

    fn call(id: &str, name: &str) -> ToolCallPayload {
        ToolCallPayload {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: deep_code_agent::ToolCallFunctionPayload {
                name: name.to_string(),
                arguments: "{\"message\":\"hi\"}".to_string(),
            },
        }
    }

    #[test]
    fn hydrate_history_keeps_assistant_tool_calls_and_results() {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "");
        record.entries.push(SessionEntry::user("hi"));
        record.entries.push(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: call("call_1", "mock_echo"),
                result: Some(ExchangeResult {
                    content: "mock_echo: hi".to_string(),
                    status: ToolResultStatus::Denied,
                }),
            }],
        ));
        record
            .entries
            .push(SessionEntry::compaction("older conversation summary", 2));
        let mut turn = deep_code_agent::TurnRecord::new("hi");
        turn.started_at_ms = 10;
        turn.finished_at_ms = Some(20);
        record.turns.push(turn);
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
        // Status now comes structurally from the exchange — no silent
        // Success fallback.
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
        record.entries.push(SessionEntry::user("hi"));
        record.entries.push(SessionEntry::assistant(
            "answer",
            Some("thinking".to_string()),
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
    fn hydrate_history_renders_pending_exchange_as_call_only() {
        // An interrupted exchange (result never recorded) shows the call but
        // fabricates no result line.
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "");
        record.entries.push(SessionEntry::user("go"));
        record.entries.push(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange::pending(call("call_1", "shell"))],
        ));

        let cells = hydrate_history(&record);
        assert!(matches!(
            &cells[1],
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "shell"
        ));
        assert!(
            !cells
                .iter()
                .any(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
        );
    }

    #[test]
    fn tool_call_renders_compact_single_line() {
        let tool = HistoryCell::ToolCall {
            tool_name: "shell".to_string(),
            arguments: "{\"command\":\n  \"grep foo\"}".to_string(),
            risk_level: None,
            requires_sandbox: None,
            approval: ToolApprovalState::NotRequired,
        };
        let lines = tool.lines(Lang::Zh);
        assert_eq!(lines.len(), 1, "tool call must be one line");
        assert!(lines[0].starts_with("shell  "));
        // Whitespace/newlines collapsed; no Risk/Approval/Sandbox noise.
        assert!(!lines[0].contains('\n'));
        assert!(!lines[0].contains("Risk"));
        assert!(!lines[0].contains('['), "ungated call carries no badge");

        let gated = HistoryCell::ToolCall {
            tool_name: "write_file".to_string(),
            arguments: "{}".to_string(),
            risk_level: Some("Medium".to_string()),
            requires_sandbox: Some(false),
            approval: ToolApprovalState::Approved,
        };
        assert!(gated.lines(Lang::Zh)[0].ends_with("[已批准]"));
        assert!(gated.lines(Lang::En)[0].ends_with("[approved]"));
    }

    #[test]
    fn tool_result_renders_compact_single_line() {
        let result = HistoryCell::ToolResult {
            tool_name: "shell".to_string(),
            status: ToolResultStatus::Success,
            summary: "ok\nmulti\nline".to_string(),
        };
        let lines = result.lines(Lang::Zh);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("ok multi line"));
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
        assert!(
            tool.lines(Lang::Zh)
                .iter()
                .any(|line| line.contains("(truncated)"))
        );

        let approval = HistoryCell::Approval {
            tool_name: "shell".to_string(),
            description: long.clone(),
            risk_level: "High".to_string(),
            requires_sandbox: true,
            matched_rule: Some("shell".to_string()),
            arguments: long,
        };
        let lines = approval.lines(Lang::Zh);
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
            cell.lines(Lang::Zh)
                .iter()
                .any(|line| line == "恢复: /restore checkpoint_1")
        );
        assert!(
            cell.lines(Lang::En)
                .iter()
                .any(|line| line == "Restore: /restore checkpoint_1")
        );
    }
}
