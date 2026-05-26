//! Basic transcript compaction for long DeepSeek sessions.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::message::{Message, Role};
use crate::model_registry::{compaction_threshold_for_model, context_window_for_model};

const RECENT_TAIL: usize = 8;

/// Effective compaction threshold: env override or 80% of model context window.
#[must_use]
pub fn effective_compaction_threshold(model: &str, override_tokens: Option<u32>) -> u32 {
    override_tokens.unwrap_or_else(|| compaction_threshold_for_model(model))
}

#[must_use]
pub fn context_usage_percent(estimated_tokens: u32, model: &str) -> u8 {
    let window = context_window_for_model(model);
    if window == 0 {
        return 0;
    }
    ((u64::from(estimated_tokens) * 100) / u64::from(window)).min(100) as u8
}

#[must_use]
pub fn near_compaction_threshold(
    model: &str,
    messages: &[Message],
    override_tokens: Option<u32>,
) -> bool {
    let threshold = effective_compaction_threshold(model, override_tokens);
    let estimated = estimate_token_count(messages);
    estimated >= threshold.saturating_mul(80) / 100
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub messages: Vec<Message>,
    pub summary: String,
    pub archived_count: usize,
}

#[must_use]
pub fn estimate_token_count(messages: &[Message]) -> u32 {
    let chars = messages
        .iter()
        .map(|message| message.content.chars().count())
        .sum::<usize>();
    (chars / 4).max(1) as u32
}

#[must_use]
pub fn should_compact(model: &str, messages: &[Message], override_tokens: Option<u32>) -> bool {
    let threshold = effective_compaction_threshold(model, override_tokens);
    estimate_token_count(messages) >= threshold
}

#[must_use]
pub fn stable_prefix_fingerprint(messages: &[Message]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let end = messages.len().saturating_sub(1);
    for message in &messages[..end] {
        format!("{:?}:{}", message.role, message.content).hash(&mut hasher);
    }
    hasher.finish()
}

/// Keep the first system prompt, summarize archived middle turns, retain recent tail.
#[must_use]
pub fn compact_messages(messages: &[Message]) -> CompactionResult {
    if messages.len() <= RECENT_TAIL + 1 {
        return CompactionResult {
            messages: messages.to_vec(),
            summary: String::new(),
            archived_count: 0,
        };
    }

    let system = messages
        .first()
        .filter(|message| matches!(message.role, Role::System))
        .cloned();
    let tail_start = messages.len().saturating_sub(RECENT_TAIL);
    let head_offset = if system.is_some() { 1 } else { 0 };
    if tail_start <= head_offset {
        return CompactionResult {
            messages: messages.to_vec(),
            summary: String::new(),
            archived_count: 0,
        };
    }

    let archived = &messages[head_offset..tail_start];
    let summary = summarize_archived(archived);
    let mut out = Vec::with_capacity(2 + RECENT_TAIL);
    if let Some(system_message) = system {
        out.push(system_message);
    }
    out.push(Message::system(format!(
        "[会话摘要 / session summary]\n{summary}"
    )));
    out.extend_from_slice(&messages[tail_start..]);

    CompactionResult {
        archived_count: archived.len(),
        messages: out,
        summary,
    }
}

fn summarize_archived(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::System => "系统",
            Role::Tool => "工具",
        };
        let snippet = truncate_chars(&message.content, 160);
        if !snippet.is_empty() {
            lines.push(format!("- {role}: {snippet}"));
        }
    }
    if lines.is_empty() {
        return "（无历史内容）".to_string();
    }
    lines.join("\n")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_chars).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_registry::DEEPSEEK_V4_PRO;

    #[test]
    fn compact_keeps_recent_tail() {
        let mut messages = vec![Message::system("sys")];
        for index in 0..12 {
            messages.push(Message::user(format!("u{index}")));
            messages.push(Message::assistant(format!("a{index}")));
        }
        let result = compact_messages(&messages);
        assert!(result.archived_count > 0);
        assert!(result.messages.len() < messages.len());
        assert!(
            result
                .messages
                .iter()
                .any(|message| message.content.contains("会话摘要"))
        );
        assert_eq!(result.messages.last().unwrap().content, "a11");
    }

    #[test]
    fn compaction_threshold_override() {
        let mut messages = vec![Message::system("sys")];
        messages.push(Message::user("x".repeat(400)));
        assert!(!should_compact(DEEPSEEK_V4_PRO, &messages, None));
        assert!(should_compact(DEEPSEEK_V4_PRO, &messages, Some(50)));
    }

    #[test]
    fn near_compaction_at_eighty_percent() {
        let messages = vec![Message::user("x".repeat(400))];
        assert!(near_compaction_threshold(
            DEEPSEEK_V4_PRO,
            &messages,
            Some(100)
        ));
    }

    #[test]
    fn prefix_fingerprint_ignores_last_message() {
        let first = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::user("two"),
        ];
        let second = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::user("three"),
        ];
        assert_eq!(
            stable_prefix_fingerprint(&first),
            stable_prefix_fingerprint(&second)
        );
    }
}
