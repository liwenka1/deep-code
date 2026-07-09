//! Basic transcript compaction for long DeepSeek sessions.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::message::{Message, Role};
use crate::model_registry::{compaction_threshold_for_model, context_window_for_model};
use crate::session::entry_wire_messages;
use crate::session_entry::{EntryKind, SessionEntry};

/// How many trailing entries survive compaction. Entries are atomic (an
/// assistant entry carries its whole tool batch), so unlike the old
/// message-based tail this can never sever a `tool_calls`/`tool` pair.
const RECENT_TAIL_ENTRIES: usize = 5;

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
    pub entries: Vec<SessionEntry>,
    pub summary: String,
    pub archived_count: usize,
}

/// Rough token estimate. ASCII/Latin text is ~4 chars per token, but CJK text
/// is closer to ~1 token per character — counting it as `chars/4` (the old
/// behavior) underestimated Chinese context by 3–4×, tripping compaction far
/// too late and skewing the cost/usage display for DeepSeek's main audience.
#[must_use]
pub fn estimate_token_count(messages: &[Message]) -> u32 {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for message in messages {
        for ch in message.content.chars() {
            if is_cjk(ch) {
                cjk += 1;
            } else {
                other += 1;
            }
        }
    }
    // CJK ≈ 1 token/char; other text ≈ 4 chars/token.
    (cjk + other / 4).max(1) as u32
}

/// Whether a character is CJK-ish (Chinese/Japanese/Korean script or wide
/// punctuation), for the per-character token estimate.
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F      // CJK symbols & punctuation
        | 0x3040..=0x30FF    // Hiragana + Katakana
        | 0x3400..=0x4DBF    // CJK Unified Ideographs Ext A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFF00..=0xFFEF    // Halfwidth/Fullwidth forms
        | 0x20000..=0x2FFFF  // CJK Unified Ideographs Ext B–F
    )
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

/// Keep the leading system entry, summarize archived middle entries into one
/// Compaction entry, retain the recent tail. `archived_count` counts entries.
#[must_use]
pub fn compact_entries(entries: &[SessionEntry]) -> CompactionResult {
    let unchanged = || CompactionResult {
        entries: entries.to_vec(),
        summary: String::new(),
        archived_count: 0,
    };
    if entries.len() <= RECENT_TAIL_ENTRIES + 1 {
        return unchanged();
    }

    let system = entries
        .first()
        .filter(|entry| matches!(entry.kind, EntryKind::System { .. }))
        .cloned();
    let tail_start = entries.len().saturating_sub(RECENT_TAIL_ENTRIES);
    let head_offset = usize::from(system.is_some());
    if tail_start <= head_offset {
        return unchanged();
    }

    let archived = &entries[head_offset..tail_start];
    // Cache-aware summary fold. DeepSeek prompt caching is automatic longest-
    // prefix matching, so a byte-stable `[system, summary…]` prefix lets the
    // cache warmed right after one compaction survive the NEXT compaction
    // instead of a full miss every time. When the archived range already opens
    // with a prior compaction summary, carry that text forward verbatim as the
    // PREFIX of the new summary (appending only the freshly-archived tail),
    // rather than re-summarizing it into a lossy `- 系统: …` line that would
    // rewrite the prefix bytes. Still destructive — only the summary text is
    // additive; the archived entries are dropped as before.
    let summary = match archived.split_first() {
        Some((first, rest)) => match &first.kind {
            EntryKind::Compaction { summary: base, .. } if rest.is_empty() => base.clone(),
            EntryKind::Compaction { summary: base, .. } => {
                format!("{base}\n{}", summarize_archived_entries(rest))
            }
            _ => summarize_archived_entries(archived),
        },
        None => summarize_archived_entries(archived),
    };
    let mut out = Vec::with_capacity(2 + RECENT_TAIL_ENTRIES);
    if let Some(system_entry) = system {
        out.push(system_entry);
    }
    out.push(SessionEntry::compaction(summary.clone(), archived.len()));
    out.extend_from_slice(&entries[tail_start..]);

    CompactionResult {
        archived_count: archived.len(),
        entries: out,
        summary,
    }
}

/// Summarize archived entries via their derived wire messages, so the summary
/// text stays byte-equivalent with the old message-based path.
fn summarize_archived_entries(entries: &[SessionEntry]) -> String {
    let wire: Vec<Message> = entries.iter().flat_map(entry_wire_messages).collect();
    summarize_archived(&wire)
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
        let mut entries = vec![SessionEntry::system("sys")];
        for index in 0..12 {
            entries.push(SessionEntry::user(format!("u{index}")));
            entries.push(SessionEntry::assistant(
                format!("a{index}"),
                None,
                Vec::new(),
            ));
        }
        let result = compact_entries(&entries);
        assert!(result.archived_count > 0);
        assert!(result.entries.len() < entries.len());
        assert!(
            result
                .entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        );
        assert!(matches!(
            &result.entries.last().unwrap().kind,
            EntryKind::Assistant { content, .. } if content == "a11"
        ));
        // The tail keeps whole entries: the derived wire still pairs cleanly.
        let wire: Vec<Message> = result
            .entries
            .iter()
            .flat_map(entry_wire_messages)
            .collect();
        assert!(
            wire.iter()
                .any(|message| message.content.contains("会话摘要"))
        );
    }

    #[test]
    fn appending_entries_preserves_wire_prefix() {
        // The invariant DeepSeek's automatic prefix cache depends on: appending
        // a turn must never rewrite earlier wire bytes, or every turn misses.
        // This guards any future refactor of the wire derivation.
        let mut entries = vec![SessionEntry::system("sys"), SessionEntry::user("hi")];
        let before: Vec<Message> = entries.iter().flat_map(entry_wire_messages).collect();
        entries.push(SessionEntry::assistant("there", None, Vec::new()));
        let after: Vec<Message> = entries.iter().flat_map(entry_wire_messages).collect();
        assert!(after.len() > before.len());
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "append must not rewrite earlier wire messages"
        );
    }

    #[test]
    fn recompaction_carries_prior_summary_as_stable_prefix() {
        // Compact once, grow past threshold, compact again: the second summary
        // must keep the first summary's text as a byte prefix so the automatic
        // prefix cache survives the second compaction instead of a full miss.
        let mut entries = vec![SessionEntry::system("sys")];
        for index in 0..12 {
            entries.push(SessionEntry::user(format!("u{index}")));
            entries.push(SessionEntry::assistant(
                format!("a{index}"),
                None,
                Vec::new(),
            ));
        }
        let first = compact_entries(&entries);
        assert!(first.archived_count > 0 && !first.summary.is_empty());

        let mut grown = first.entries.clone();
        for index in 12..24 {
            grown.push(SessionEntry::user(format!("u{index}")));
            grown.push(SessionEntry::assistant(
                format!("a{index}"),
                None,
                Vec::new(),
            ));
        }
        let second = compact_entries(&grown);
        assert!(second.archived_count > 0);
        assert!(
            second.summary.starts_with(&first.summary),
            "the re-compaction summary must extend the prior one as a stable prefix"
        );
    }

    #[test]
    fn cjk_text_is_not_underestimated() {
        // 100 Han chars ≈ ~100 tokens (not 25 like the old chars/4); ASCII stays ~/4.
        let han = "\u{5b57}".repeat(100);
        let ascii = "a".repeat(100);
        let cjk_tokens = estimate_token_count(&[Message::user(han)]);
        let ascii_tokens = estimate_token_count(&[Message::user(ascii)]);
        assert!(
            cjk_tokens >= 90,
            "CJK should count ~1 token/char, got {cjk_tokens}"
        );
        assert!(
            ascii_tokens <= 30,
            "ASCII should stay ~chars/4, got {ascii_tokens}"
        );
        assert!(cjk_tokens > ascii_tokens);
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
