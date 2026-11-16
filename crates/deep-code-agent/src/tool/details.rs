//! Retention of UI previews, independent of model context and turn boundaries.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::session_entry::{EntryKind, SessionEntry};

/// Recent display payload may use this much space in the serialized session.
/// Execution facts and source references remain even after a preview expires.
pub(crate) const PREVIEW_RETENTION_BYTES: usize = 1024 * 1024;

pub(crate) fn storage_size(details: &Value) -> usize {
    #[cfg(test)]
    SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let json = serde_json::to_vec_pretty(details).expect("JSON values serialize");
    // Account for indentation where details sit inside entries/exchanges/result.
    json.len().saturating_add(
        json.iter()
            .filter(|byte| **byte == b'\n')
            .count()
            .saturating_mul(32),
    )
}

fn fields(value: &Value, names: &[&str]) -> Map<String, Value> {
    names
        .iter()
        .filter_map(|name| {
            value
                .get(*name)
                .map(|value| ((*name).into(), value.clone()))
        })
        .collect()
}

/// Preserve the facts consumed by execution summaries; display payload and
/// unknown schemas remain available through the complete details source.
pub(crate) fn compact_details(details: &Value) -> Value {
    let mut compact = fields(
        details,
        &[
            "job_id",
            "kind",
            "command",
            "command_truncated",
            "cwd",
            "cwd_truncated",
            "status",
            "exit_code",
            "duration_ms",
            "sandboxed",
            "network",
            "stdin_open",
            "input_error",
            "agent_id",
            "name",
            "name_truncated",
            "role",
            "steps",
            "details_storage",
            "content_storage",
        ],
    );
    for (name, limit) in [("command", 512), ("cwd", 256), ("name", 512)] {
        if let Some(Value::String(text)) = compact.get_mut(name)
            && text.chars().count() > limit
        {
            *text = crate::text_util::clip_middle(text, limit, limit / 3, limit / 3, |_| {
                "\n...[details preview omitted]...\n".into()
            });
            compact.insert(format!("{name}_truncated"), true.into());
        }
    }
    for name in ["stdout", "stderr"] {
        if let Some(stream) = details.get(name) {
            compact.insert(
                name.into(),
                fields(
                    stream,
                    &[
                        "total_bytes",
                        "omitted_bytes",
                        "inline_omitted_bytes",
                        "inline_truncated",
                        "finished",
                        "read_error",
                        "capture_complete",
                        "spill",
                    ],
                )
                .into(),
            );
        }
    }
    if let Some(page) = details.get("output_page") {
        compact.insert(
            "output_page".into(),
            fields(
                page,
                &[
                    "stream",
                    "requested_start_byte",
                    "start_byte",
                    "end_byte",
                    "retained_start_byte",
                    "unavailable_before_byte",
                    "total_bytes",
                    "next_start_byte",
                    "has_more",
                    "finished",
                    "lossy_utf8",
                    "read_error",
                ],
            )
            .into(),
        );
    }
    if let Some(edit) = details.get("edit") {
        let mut facts = fields(
            edit,
            &[
                "path",
                "path_truncated",
                "added",
                "removed",
                "truncated",
                "new_file",
            ],
        );
        if let Some(Value::String(path)) = facts.get_mut("path")
            && path.chars().count() > 512
        {
            *path = crate::text_util::clip_middle(path, 512, 170, 170, |_| {
                "\n...[details preview omitted]...\n".into()
            });
            facts.insert("path_truncated".into(), true.into());
        }
        if edit
            .get("diff")
            .and_then(Value::as_str)
            .is_some_and(|diff| !diff.is_empty())
        {
            facts.insert("truncated".into(), true.into());
        }
        compact.insert("edit".into(), facts.into());
    }
    compact.into()
}

/// The recoverable display portion, including JSON escaping and indentation.
pub(crate) fn preview_size(details: &Value) -> usize {
    storage_size(details).saturating_sub(storage_size(&compact_details(details)))
}

/// Prepared once, outside the runtime state lock, together with source capture.
#[derive(Debug, Clone)]
pub(crate) struct PreparedPreview {
    bytes: usize,
    compact: Value,
}

impl PreparedPreview {
    pub(crate) fn new(details: &Value) -> Option<Self> {
        // A value that has never attempted source capture may be the only
        // complete copy. Keep it intact until a source can be prepared.
        details.get("details_storage")?.as_object()?;
        let mut compact = compact_details(details);
        let bytes = storage_size(details).saturating_sub(storage_size(&compact));
        if bytes == 0 {
            return None;
        }
        compact["details_storage"]["inline_truncated"] = true.into();
        Some(Self { bytes, compact })
    }
}

#[derive(Debug, Clone)]
struct RetainedPreview {
    entry: usize,
    exchange: usize,
    prepared: PreparedPreview,
}

/// Only active, reclaimable previews are indexed. Expired history is never
/// serialized again when a later result arrives.
#[derive(Debug, Clone, Default)]
pub(crate) struct PreviewRetention {
    bytes: usize,
    recent: VecDeque<RetainedPreview>,
}

impl PreviewRetention {
    pub(crate) fn from_entries(entries: &mut [Arc<SessionEntry>]) -> Self {
        let mut retention = Self::default();
        for (entry_index, entry) in entries.iter().enumerate() {
            let EntryKind::Assistant { exchanges, .. } = &entry.kind else {
                continue;
            };
            for (exchange_index, exchange) in exchanges.iter().enumerate() {
                if let Some(prepared) = exchange
                    .result
                    .as_ref()
                    .and_then(|result| result.details.as_ref())
                    .and_then(PreparedPreview::new)
                {
                    retention.push(entry_index, exchange_index, prepared);
                }
            }
        }
        retention.expire(entries);
        retention
    }

    fn push(&mut self, entry: usize, exchange: usize, prepared: PreparedPreview) {
        self.bytes = self.bytes.saturating_add(prepared.bytes);
        self.recent.push_back(RetainedPreview {
            entry,
            exchange,
            prepared,
        });
    }

    pub(crate) fn record(
        &mut self,
        entries: &mut [Arc<SessionEntry>],
        entry: usize,
        exchange: usize,
        prepared: Option<PreparedPreview>,
    ) {
        if let Some(prepared) = prepared {
            self.push(entry, exchange, prepared);
            self.expire(entries);
        }
    }

    pub(crate) fn inserted_entry(&mut self, index: usize) {
        for preview in &mut self.recent {
            if preview.entry >= index {
                preview.entry += 1;
            }
        }
    }

    fn expire(&mut self, entries: &mut [Arc<SessionEntry>]) {
        while self.bytes > PREVIEW_RETENTION_BYTES {
            let preview = self.recent.pop_front().expect("indexed preview bytes");
            self.bytes -= preview.prepared.bytes;
            let EntryKind::Assistant { exchanges, .. } =
                &mut Arc::make_mut(&mut entries[preview.entry]).kind
            else {
                unreachable!("preview points to an assistant entry");
            };
            exchanges[preview.exchange].result.as_mut().unwrap().details =
                Some(preview.prepared.compact);
        }
    }
}

#[cfg(test)]
thread_local! {
    static SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn serialization_count() -> usize {
    SERIALIZATIONS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_entry::{ExchangeResult, ToolExchange};
    use crate::{Content, ToolCallFunctionPayload, ToolCallPayload, ToolResultStatus};
    use serde_json::json;

    fn entry(details: Value) -> Arc<SessionEntry> {
        Arc::new(SessionEntry::assistant(
            "",
            None,
            vec![ToolExchange {
                call: ToolCallPayload {
                    id: "call".into(),
                    call_type: "function".into(),
                    function: ToolCallFunctionPayload {
                        name: "fixture".into(),
                        arguments: "{}".into(),
                    },
                },
                result: Some(ExchangeResult {
                    content: Content::from("done"),
                    details: Some(details),
                    status: ToolResultStatus::Success,
                    asked: false,
                }),
            }],
        ))
    }

    fn details(entry: &SessionEntry) -> &Value {
        let EntryKind::Assistant { exchanges, .. } = &entry.kind else {
            unreachable!();
        };
        exchanges[0]
            .result
            .as_ref()
            .unwrap()
            .details
            .as_ref()
            .unwrap()
    }

    #[test]
    fn retention_counts_escaped_payloads_and_preserves_recent_previews_and_shared_entries() {
        let original = json!({
            "edit": {"path": "a", "diff": "\u{1}\t\"界".repeat(1500),
                "added": 1, "removed": 1, "truncated": false, "new_file": false},
            "details_storage": {"path": "full.json", "complete": true,
                "original_bytes": 20000, "error": null, "inline_truncated": false},
        });
        let original_entry = entry(original.clone());
        let mut entries = vec![original_entry.clone(); 100];
        let mut shared = entries.clone();
        let wire_before = crate::session::Session::from_entries(shared.clone()).wire_messages();
        PreviewRetention::from_entries(&mut entries);
        assert_eq!(details(&original_entry), &original);
        assert!(details(&entries[0])["edit"].get("diff").is_none());
        assert_eq!(details(entries.last().unwrap()), &original);
        let cost: usize = entries
            .iter()
            .map(|entry| preview_size(details(entry)))
            .sum();
        assert!(cost <= PREVIEW_RETENTION_BYTES);
        assert!(cost > PREVIEW_RETENTION_BYTES / 2);
        PreviewRetention::from_entries(&mut shared);
        assert_eq!(entries, shared);
        assert_eq!(
            crate::session::Session::from_entries(entries.clone()).wire_messages(),
            wire_before
        );
        let saved = entries.clone();
        PreviewRetention::from_entries(&mut entries);
        assert_eq!(entries, saved, "archived previews must stay archived");
    }

    #[test]
    fn archived_execution_keeps_capture_and_pagination_facts_and_the_original_source() {
        let original = json!({
            "job_id": "job", "kind": "foreground", "command": "test", "cwd": ".",
            "status": "failed", "exit_code": 3, "duration_ms": 42, "sandboxed": true,
            "network": false, "stdin_open": false, "input_error": "write failed",
            "stdout": {"text": "captured", "total_bytes": 100, "omitted_bytes": 30,
                "inline_truncated": true, "finished": true, "read_error": null,
                "capture_complete": true, "spill": {"path": "stdout", "complete": true}},
            "stderr": {"text": "error", "total_bytes": 8, "omitted_bytes": 0,
                "inline_truncated": false, "finished": true, "read_error": "read failed",
                "capture_complete": false, "spill": null},
            "output_page": {"stream": "stdout", "text": "page", "start_byte": 30,
                "end_byte": 50, "next_start_byte": 50, "total_bytes": 100, "has_more": true,
                "finished": true, "lossy_utf8": false, "read_error": null},
            "details_storage": {"path": "full.json", "complete": true,
                "original_bytes": 9000, "error": null, "inline_truncated": false},
            "content_storage": {"path": "output.txt", "complete": true,
                "original_bytes": 10000, "error": null, "inline_truncated": true},
        });
        let archived = PreparedPreview::new(&original).unwrap().compact;
        let mut expected = original;
        for name in ["stdout", "stderr", "output_page"] {
            expected[name].as_object_mut().unwrap().remove("text");
        }
        expected["details_storage"]["inline_truncated"] = true.into();
        assert_eq!(archived, expected);
        assert_eq!(preview_size(&archived), 0);
    }

    #[test]
    fn unsourced_previews_are_preserved_and_source_capture_failures_remain_explicit() {
        for payload in [
            json!({"agent_id": "agent", "status": "completed", "steps": 1,
                "structured": {"findings": ["x".repeat(PREVIEW_RETENTION_BYTES + 1)]}}),
            json!({"new_schema": "x".repeat(PREVIEW_RETENTION_BYTES + 1)}),
        ] {
            let mut entries = vec![entry(payload.clone())];
            PreviewRetention::from_entries(&mut entries);
            assert_eq!(details(&entries[0]), &payload);
        }
        let failed = json!({"structured": "x".repeat(PREVIEW_RETENTION_BYTES + 1),
            "details_storage": {"path": null, "complete": false,
                "error": "permission denied", "inline_truncated": false}});
        let mut entries = vec![entry(failed.clone())];
        PreviewRetention::from_entries(&mut entries);
        let mut source = failed["details_storage"].clone();
        source["inline_truncated"] = true.into();
        assert_eq!(details(&entries[0])["details_storage"], source);
    }
}
