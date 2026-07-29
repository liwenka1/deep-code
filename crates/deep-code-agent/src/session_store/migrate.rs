//! One-time migration of schema-v1 session files (wire messages) into the
//! schema-v2 entry representation.
//!
//! Grouping reuses [`Session::from_wire_messages`] — the same logic the old
//! `repair_dangling_tool_calls` applied at every resume, now run exactly once
//! per legacy file. Tool-result statuses (absent from the wire) are recovered
//! from the turns' [`super::TurnRecord`] audit copies; the v1
//! `compaction: "archived=N"` metadata is folded back into the Compaction
//! entry produced by the summary-message grouping.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

use serde::Deserialize;

use crate::message::Message;
use crate::session::Session;
use crate::session_entry::EntryKind;
use crate::tool::ToolResultStatus;

use crate::model::Usage;

use super::{CheckpointRecord, SESSION_SCHEMA_VERSION, SessionId, SessionRecord, TurnRecord};

/// The v1 on-disk layout (schema_version == 1). Frozen: these shapes mirror
/// what v1 files actually contain and must not alias the live types — the
/// live schema is free to drop fields (v1's `config` snapshot and per-turn
/// `tool_results` are already gone from it) without breaking this reader.
#[derive(Debug, Deserialize)]
pub(super) struct SessionRecordV1 {
    pub id: SessionId,
    pub workspace: PathBuf,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub messages: Vec<Message>,
    pub turns: Vec<TurnRecordV1>,
    #[serde(default)]
    pub checkpoints: Vec<CheckpointRecord>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub compaction: Option<String>,
}

/// V1 turn shape: carries the audit `tool_results` the live [`TurnRecord`]
/// no longer stores; migration mines them for result statuses, then drops them.
/// Most fields exist only to consume the old wire format — the live record now
/// keeps just `started_at_ms`, so the rest are deserialized and discarded.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields consume the v1 wire format; only started_at_ms/tool_results are read
pub(super) struct TurnRecordV1 {
    pub user_prompt: String,
    #[serde(default)]
    pub tool_results: Vec<ToolResultV1>,
    pub usage: Option<Usage>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// The slice of a v1 persisted tool result that migration reads.
#[derive(Debug, Deserialize)]
pub(super) struct ToolResultV1 {
    pub call_id: String,
    pub status: ToolResultStatus,
}

pub(super) fn migrate_v1(v1: SessionRecordV1) -> SessionRecord {
    let mut entries = Session::from_wire_messages(&v1.messages).entries().to_vec();

    // Recover result statuses from the audit copies: multiple results may
    // share a call_id across turns (ids restart per provider), so consume
    // them in order per id, mirroring the old TUI pairing.
    let mut statuses: HashMap<&str, VecDeque<ToolResultStatus>> = HashMap::new();
    for result in v1.turns.iter().flat_map(|turn| &turn.tool_results) {
        statuses
            .entry(result.call_id.as_str())
            .or_default()
            .push_back(result.status);
    }
    for entry in &mut entries {
        let EntryKind::Assistant { exchanges, .. } = &mut entry.kind else {
            continue;
        };
        for exchange in exchanges {
            let Some(result) = exchange.result.as_mut() else {
                continue;
            };
            if let Some(queue) = statuses.get_mut(exchange.call.id.as_str())
                && let Some(status) = queue.pop_front()
            {
                result.status = status;
            }
        }
    }

    // Fold the v1 "archived=N" metadata into the latest Compaction entry.
    if let Some(archived) = v1
        .compaction
        .as_deref()
        .and_then(|value| value.strip_prefix("archived="))
        .and_then(|value| value.parse::<usize>().ok())
        && let Some(entry) = entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        && let EntryKind::Compaction { archived_count, .. } = &mut entry.kind
    {
        *archived_count = archived;
    }

    // v2 keeps only the turn's start timestamp; the old prompt/usage/finish
    // fields were write-only and are dropped on migration.
    let turns = v1
        .turns
        .into_iter()
        .map(|turn| TurnRecord {
            started_at_ms: turn.started_at_ms,
        })
        .collect();

    SessionRecord {
        schema_version: SESSION_SCHEMA_VERSION,
        id: v1.id,
        workspace: v1.workspace,
        created_at_ms: v1.created_at_ms,
        updated_at_ms: v1.updated_at_ms,
        entries,
        turns,
        checkpoints: v1.checkpoints,
        summary: v1.summary,
        compaction: v1.compaction,
        // v1 never tracked lifetime cost; a migrated session starts its total
        // from zero (the field is defaulted for the same reason on load).
        session_cost: crate::pricing::CostEstimate::default(),
        session_cache_hit_tokens: 0,
        session_cache_miss_tokens: 0,
        session_cache_savings: crate::pricing::CostEstimate::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
    use crate::session_entry::EntryKind;

    fn call(id: &str) -> ToolCallPayload {
        ToolCallPayload {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunctionPayload {
                name: "shell".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn v1(
        messages: Vec<Message>,
        turns: Vec<TurnRecordV1>,
        compaction: Option<&str>,
    ) -> SessionRecordV1 {
        SessionRecordV1 {
            id: SessionId("session_1_0".to_string()),
            workspace: PathBuf::from("/tmp/ws"),
            created_at_ms: 1,
            updated_at_ms: 2,
            messages,
            turns,
            checkpoints: Vec::new(),
            summary: None,
            compaction: compaction.map(str::to_string),
        }
    }

    #[test]
    fn migrates_plain_history_and_recovers_status() {
        let turn = TurnRecordV1 {
            user_prompt: "do it".to_string(),
            tool_results: vec![ToolResultV1 {
                call_id: "c1".to_string(),
                status: ToolResultStatus::Error,
            }],
            usage: None,
            started_at_ms: 1,
            finished_at_ms: Some(2),
        };
        let record = migrate_v1(v1(
            vec![
                Message::system("sys"),
                Message::user("do it"),
                Message::assistant_with_tool_calls("", vec![call("c1")]),
                Message::tool("c1", "boom"),
            ],
            vec![turn],
            None,
        ));

        assert_eq!(record.schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(record.entries.len(), 3);
        let EntryKind::Assistant { exchanges, .. } = &record.entries[2].kind else {
            panic!("expected assistant entry");
        };
        let result = exchanges[0].result.as_ref().unwrap();
        assert_eq!(result.content, "boom");
        assert_eq!(result.status, ToolResultStatus::Error);
    }

    #[test]
    fn migrates_dangling_call_to_pending_exchange() {
        let record = migrate_v1(v1(
            vec![
                Message::user("go"),
                Message::assistant_with_tool_calls("", vec![call("c1")]),
                // interrupted: no tool message persisted
            ],
            Vec::new(),
            None,
        ));
        let EntryKind::Assistant { exchanges, .. } = &record.entries[1].kind else {
            panic!("expected assistant entry");
        };
        assert!(exchanges[0].result.is_none());
        // The derived wire still satisfies the protocol.
        let wire = Session::from_entries(record.entries.clone()).wire_messages();
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn migrates_compaction_summary_with_archived_count() {
        let record = migrate_v1(v1(
            vec![
                Message::system("sys"),
                Message::system("[会话摘要 / session summary]\n- 用户: 旧的".to_string()),
                Message::user("new"),
            ],
            Vec::new(),
            Some("archived=7"),
        ));
        assert!(matches!(
            &record.entries[1].kind,
            EntryKind::Compaction { summary, archived_count }
                if summary == "- 用户: 旧的" && *archived_count == 7
        ));
    }
}
