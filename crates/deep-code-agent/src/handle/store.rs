use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque handle identifier returned to the parent model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HandleId(pub String);

impl HandleId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleKind {
    Transcript,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleSummary {
    pub id: HandleId,
    pub kind: HandleKind,
    pub summary: String,
    pub byte_len: usize,
    pub line_count: usize,
}

#[derive(Debug, Clone)]
struct StoredHandle {
    kind: HandleKind,
    payload: Value,
    text: String,
}

/// In-memory handle store. Roadmap 10 will add bounded reads and persistence.
#[derive(Debug, Default)]
pub struct HandleStore {
    next_id: AtomicU64,
    handles: HashMap<String, StoredHandle>,
}

impl HandleStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_json(
        &mut self,
        key_hint: impl Into<String>,
        kind: HandleKind,
        payload: Value,
    ) -> HandleSummary {
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        let id = HandleId(format!(
            "h_{}_{}",
            sanitize_hint(&key_hint.into()),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let byte_len = text.len();
        let line_count = text.lines().count();
        let summary = summarize_text(&text);
        self.handles.insert(
            id.0.clone(),
            StoredHandle {
                kind,
                payload,
                text,
            },
        );
        HandleSummary {
            id,
            kind: HandleKind::Transcript,
            summary,
            byte_len,
            line_count,
        }
    }

    #[must_use]
    pub fn get_summary(&self, id: &HandleId) -> Option<HandleSummary> {
        let stored = self.handles.get(id.as_str())?;
        Some(HandleSummary {
            id: id.clone(),
            kind: stored.kind.clone(),
            summary: summarize_text(&stored.text),
            byte_len: stored.text.len(),
            line_count: stored.text.lines().count(),
        })
    }

    pub fn read_head(&self, id: &HandleId, max_lines: usize) -> Option<String> {
        let stored = self.handles.get(id.as_str())?;
        Some(
            stored
                .text
                .lines()
                .take(max_lines)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    pub fn read_tail(&self, id: &HandleId, max_lines: usize) -> Option<String> {
        let stored = self.handles.get(id.as_str())?;
        let lines: Vec<&str> = stored.text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        Some(lines[start..].join("\n"))
    }

    #[must_use]
    pub fn get_payload(&self, id: &HandleId) -> Option<&Value> {
        self.handles.get(id.as_str()).map(|stored| &stored.payload)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleRecord {
    pub id: HandleId,
    pub kind: HandleKind,
    pub summary: String,
    pub byte_len: usize,
    pub line_count: usize,
}

impl From<HandleSummary> for HandleRecord {
    fn from(value: HandleSummary) -> Self {
        Self {
            id: value.id,
            kind: value.kind,
            summary: value.summary,
            byte_len: value.byte_len,
            line_count: value.line_count,
        }
    }
}

fn sanitize_hint(hint: &str) -> String {
    hint.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

fn summarize_text(text: &str) -> String {
    const MAX: usize = 160;
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX {
        flattened
    } else {
        format!(
            "{}...",
            flattened.chars().take(MAX).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insert_and_read_handle_ranges() {
        let mut store = HandleStore::new();
        let payload = json!({"lines": ["a", "b", "c", "d"]});
        let summary = store.insert_json("agent:test", HandleKind::Transcript, payload);
        assert!(summary.id.as_str().starts_with("h_agent_test_"));
        assert_eq!(store.read_head(&summary.id, 2).unwrap(), "{\n  \"lines\": [");
        assert!(store.get_summary(&summary.id).unwrap().line_count > 1);
    }
}
