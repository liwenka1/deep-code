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

/// Symbolic handle reference for richer analysis backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarHandle {
    pub kind: String,
    pub session_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub length: usize,
    pub repr_preview: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// deep-code extension: opaque store id (same as `name` when using id-as-name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl VarHandle {
    #[must_use]
    pub fn from_summary(summary: &HandleSummary, session_id: impl Into<String>) -> Self {
        Self {
            kind: "var_handle".to_string(),
            session_id: session_id.into(),
            name: summary.id.as_str().to_string(),
            type_name: format!("{:?}", summary.kind).to_lowercase(),
            length: summary.byte_len,
            repr_preview: summary.summary.clone(),
            sha256: String::new(),
            id: Some(summary.id.as_str().to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleKind {
    Transcript,
    RlmResult,
    Artifact,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleSummary {
    pub id: HandleId,
    pub kind: HandleKind,
    pub summary: String,
    pub byte_len: usize,
    pub line_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_owner: Option<String>,
}

#[derive(Debug, Clone)]
struct StoredHandle {
    kind: HandleKind,
    session_owner: Option<String>,
    payload: Option<Value>,
    text: String,
}

/// In-memory handle store for large tool outputs and RLM artifacts.
#[derive(Debug, Default)]
pub struct HandleStore {
    next_id: AtomicU64,
    handles: HashMap<String, StoredHandle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleCount {
    pub byte_len: usize,
    pub line_count: usize,
    pub char_count: usize,
    pub kind: HandleKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleReadOutput {
    pub mode: String,
    pub handle_id: String,
    pub content: Option<String>,
    pub truncated: bool,
    pub count: Option<HandleCount>,
    pub summary: Option<HandleSummary>,
}

impl HandleStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_text(
        &mut self,
        key_hint: impl Into<String>,
        kind: HandleKind,
        text: String,
        session_owner: Option<String>,
    ) -> HandleSummary {
        let id = self.next_id_for(&key_hint.into());
        let byte_len = text.len();
        let line_count = text.lines().count();
        let summary = summarize_text(&text);
        self.handles.insert(
            id.0.clone(),
            StoredHandle {
                kind: kind.clone(),
                session_owner: session_owner.clone(),
                payload: None,
                text,
            },
        );
        HandleSummary {
            id,
            kind,
            summary,
            byte_len,
            line_count,
            session_owner,
        }
    }

    pub fn insert_json(
        &mut self,
        key_hint: impl Into<String>,
        kind: HandleKind,
        payload: Value,
    ) -> HandleSummary {
        self.insert_json_with_owner(key_hint, kind, payload, None)
    }

    pub fn insert_json_with_owner(
        &mut self,
        key_hint: impl Into<String>,
        kind: HandleKind,
        payload: Value,
        session_owner: Option<String>,
    ) -> HandleSummary {
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        let id = self.next_id_for(&key_hint.into());
        let byte_len = text.len();
        let line_count = text.lines().count();
        let summary = summarize_text(&text);
        let kind_clone = kind.clone();
        self.handles.insert(
            id.0.clone(),
            StoredHandle {
                kind,
                session_owner: session_owner.clone(),
                payload: Some(payload),
                text,
            },
        );
        HandleSummary {
            id,
            kind: kind_clone,
            summary,
            byte_len,
            line_count,
            session_owner,
        }
    }

    pub fn resolve_id(&self, raw: &str) -> Option<HandleId> {
        if self.handles.contains_key(raw) {
            return Some(HandleId(raw.to_string()));
        }
        if let Some((session_id, name)) = raw.rsplit_once('/') {
            self.handles.iter().find_map(|(id, stored)| {
                stored
                    .session_owner
                    .as_deref()
                    .filter(|owner| *owner == session_id)
                    .and_then(|_| {
                        if id == name || id.ends_with(name) {
                            Some(HandleId(id.clone()))
                        } else {
                            None
                        }
                    })
            })
        } else {
            None
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
            session_owner: stored.session_owner.clone(),
        })
    }

    #[must_use]
    pub fn count(&self, id: &HandleId) -> Option<HandleCount> {
        let stored = self.handles.get(id.as_str())?;
        Some(HandleCount {
            byte_len: stored.text.len(),
            line_count: stored.text.lines().count(),
            char_count: stored.text.chars().count(),
            kind: stored.kind.clone(),
        })
    }

    pub fn read_head(
        &self,
        id: &HandleId,
        max_lines: usize,
        max_chars: usize,
    ) -> Option<(String, bool)> {
        let stored = self.handles.get(id.as_str())?;
        let lines: Vec<&str> = stored.text.lines().collect();
        let selected = lines
            .into_iter()
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        Some(truncate_chars(selected, max_chars))
    }

    pub fn read_tail(
        &self,
        id: &HandleId,
        max_lines: usize,
        max_chars: usize,
    ) -> Option<(String, bool)> {
        let stored = self.handles.get(id.as_str())?;
        let lines: Vec<&str> = stored.text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        let selected = lines[start..].join("\n");
        Some(truncate_chars(selected, max_chars))
    }

    pub fn read_lines(
        &self,
        id: &HandleId,
        start_line: usize,
        end_line: usize,
        max_chars: usize,
    ) -> Option<(String, bool)> {
        let stored = self.handles.get(id.as_str())?;
        if start_line == 0 || end_line < start_line {
            return Some((String::new(), false));
        }
        let lines: Vec<&str> = stored.text.lines().collect();
        let start = start_line.saturating_sub(1);
        let end = end_line.min(lines.len());
        if start >= lines.len() {
            return Some((String::new(), false));
        }
        let selected = lines[start..end].join("\n");
        Some(truncate_chars(selected, max_chars))
    }

    #[must_use]
    pub fn get_payload(&self, id: &HandleId) -> Option<&Value> {
        self.handles
            .get(id.as_str())
            .and_then(|stored| stored.payload.as_ref())
    }

    pub fn purge_session(&mut self, session_id: &str) -> usize {
        let keys: Vec<String> = self
            .handles
            .iter()
            .filter_map(|(id, stored)| {
                stored
                    .session_owner
                    .as_deref()
                    .filter(|owner| *owner == session_id)
                    .map(|_| id.clone())
            })
            .collect();
        for key in &keys {
            self.handles.remove(key);
        }
        keys.len()
    }

    fn next_id_for(&self, key_hint: &str) -> HandleId {
        HandleId(format!(
            "h_{}_{}",
            sanitize_hint(key_hint),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
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

fn truncate_chars(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    (text.chars().take(max_chars).collect::<String>(), true)
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
        format!("{}...", flattened.chars().take(MAX).collect::<String>())
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
        let (head, truncated) = store.read_head(&summary.id, 2, 10_000).unwrap();
        assert!(!truncated);
        assert_eq!(head, "{\n  \"lines\": [");
        assert!(store.get_summary(&summary.id).unwrap().line_count > 1);
    }

    #[test]
    fn read_lines_and_count() {
        let mut store = HandleStore::new();
        let summary = store.insert_text(
            "rlm:out",
            HandleKind::RlmResult,
            "one\ntwo\nthree\n".to_string(),
            Some("sess".to_string()),
        );
        let (slice, _) = store.read_lines(&summary.id, 2, 2, 10_000).unwrap();
        assert_eq!(slice, "two");
        let count = store.count(&summary.id).unwrap();
        assert_eq!(count.line_count, 3);
    }

    #[test]
    fn purge_session_removes_owned_handles() {
        let mut store = HandleStore::new();
        let summary = store.insert_text(
            "rlm:out",
            HandleKind::RlmResult,
            "data".to_string(),
            Some("rlm_a".to_string()),
        );
        assert_eq!(store.purge_session("rlm_a"), 1);
        assert!(store.get_summary(&summary.id).is_none());
    }
}
