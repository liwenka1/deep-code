use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::content::Content;
use crate::image::{ImageAsset, ImageRef};
use crate::message::Message;
use crate::model::ToolCallPayload;
use crate::runtime::TurnId;
use crate::session_entry::{Ask, EntryKind, ExchangeResult, SessionEntry, ToolExchange};
use crate::tool::ToolResultStatus;

/// A missing settlement cannot establish whether external actions happened.
/// Recovery describes the uncertainty and never repeats the operation itself.
pub(crate) const INTERRUPTED_TOOL_RESULT: &str = "工具执行结果未知：会话在结果记录前中断。先验证外部状态，不要直接重跑可能已执行的操作。/ Tool outcome unknown: the session ended before its result was recorded. Verify external state before repeating an operation that may already have run.";

/// Marker prefixing the derived compaction-summary system message. Shared
/// with the wire→entry grouping so summaries round-trip into
/// [`EntryKind::Compaction`].
pub(crate) const COMPACTION_SUMMARY_PREFIX: &str = "[会话摘要 / session summary]\n";

/// Domain-level conversation: an ordered list of [`SessionEntry`] values.
///
/// The DeepSeek wire messages are *derived* via [`Session::wire_messages`],
/// which is where protocol invariants (every `tool_calls` id followed by a
/// `role=tool` message) are enforced by construction — there is no
/// after-the-fact repair step.
///
/// Entries are held behind `Arc` so the live session and the persistence
/// record share one transcript: `persist()` copies a vector of pointers, not
/// every entry's bytes. Entries are append-mostly; the one in-place mutation
/// (a tool result landing in its exchange) goes through [`Arc::make_mut`],
/// which clones only that single entry when the record still references it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    entries: Vec<Arc<SessionEntry>>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_entries(entries: Vec<Arc<SessionEntry>>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[Arc<SessionEntry>] {
        &self.entries
    }

    pub fn replace_entries(&mut self, entries: Vec<Arc<SessionEntry>>) {
        self.entries = entries;
    }

    fn push_entry(&mut self, entry: SessionEntry) {
        self.entries.push(Arc::new(entry));
    }

    pub fn push_system(&mut self, content: impl Into<String>) {
        self.push_entry(SessionEntry::system(content));
    }

    pub fn push_user(&mut self, turn_id: TurnId, content: impl Into<Content>) {
        self.push_entry(SessionEntry::user(content).with_turn_id(turn_id));
    }

    /// Append a user turn that carries images.
    ///
    /// Assets are admitted before they enter the conversation.
    pub fn push_user_with_images(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
        images: Vec<ImageAsset>,
    ) {
        self.push_entry(SessionEntry::user_with_images(content, images).with_turn_id(turn_id));
    }

    /// Append an assistant turn; `calls` become pending exchanges whose
    /// results are filled in by [`Session::record_tool_result`].
    pub fn push_assistant(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
        reasoning: impl Into<String>,
        calls: Vec<ToolCallPayload>,
    ) {
        let reasoning = reasoning.into();
        let exchanges = calls.into_iter().map(ToolExchange::pending).collect();
        self.push_entry(
            SessionEntry::assistant(
                content,
                (!reasoning.is_empty()).then_some(reasoning),
                exchanges,
            )
            .with_turn_id(turn_id),
        );
    }

    /// Keep a late partial reply beside the request it answered. A newer turn
    /// may already have appended its user message. If compaction replaced the
    /// anchor, discard the fragment instead of attaching it to a different turn.
    pub(crate) fn insert_partial_assistant_after(
        &mut self,
        anchor: &Arc<SessionEntry>,
        turn_id: TurnId,
        content: String,
        reasoning: String,
    ) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| Arc::ptr_eq(entry, anchor))
        else {
            return false;
        };
        let reply =
            SessionEntry::assistant(content, Some(reasoning), Vec::new()).with_turn_id(turn_id);
        self.entries.insert(index + 1, Arc::new(reply));
        true
    }

    /// Record a tool result into the newest assistant entry's matching
    /// pending exchange. Calls always belong to the latest assistant entry
    /// (the runtime pushes the entry immediately before executing its batch).
    /// Returns false when no pending exchange matches.
    ///
    /// `ask` is whether the approval gate put this call in front of a human —
    /// see [`ExchangeResult::asked`]. The completed current record always
    /// stores this decision.
    pub fn record_tool_result(
        &mut self,
        call_id: &str,
        content: Content,
        status: ToolResultStatus,
        ask: Ask,
        details: Option<serde_json::Value>,
    ) -> bool {
        // Find the newest assistant entry first, and only `make_mut` (which
        // clones the entry while the persistence record also holds it) once a
        // matching pending exchange is confirmed — a miss must stay copy-free.
        let latest_assistant = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.kind, EntryKind::Assistant { .. }));
        let Some(entry) = latest_assistant else {
            return false;
        };
        let EntryKind::Assistant { exchanges, .. } = &entry.kind else {
            unreachable!("filtered to assistant entries above");
        };
        if !exchanges
            .iter()
            .any(|exchange| exchange.call.id == call_id && exchange.result.is_none())
        {
            return false;
        }
        let EntryKind::Assistant { exchanges, .. } = &mut Arc::make_mut(entry).kind else {
            unreachable!("make_mut preserves the entry kind");
        };
        let Some(exchange) = exchanges
            .iter_mut()
            .find(|exchange| exchange.call.id == call_id && exchange.result.is_none())
        else {
            unreachable!("presence checked above");
        };
        exchange.result = Some(ExchangeResult {
            content,
            details,
            status,
            asked: ask == Ask::Asked,
        });
        true
    }

    /// Whether this turn contains visual observations still relevant to its work.
    pub(crate) fn turn_has_images(&self, turn_id: &TurnId) -> bool {
        self.entries
            .iter()
            .filter(|entry| entry.turn_id.as_ref() == Some(turn_id))
            .any(|entry| match &entry.kind {
                EntryKind::User { content } => content.has_images(),
                EntryKind::Assistant { exchanges, .. } => exchanges.iter().any(|exchange| {
                    exchange
                        .result
                        .as_ref()
                        .is_some_and(|result| result.content.has_images())
                }),
                _ => false,
            })
    }

    /// Derive the DeepSeek/OpenAI wire messages. Pending exchanges emit the
    /// interrupted placeholder, so the protocol invariant (assistant
    /// `tool_calls` followed by one `role=tool` message per id) holds by
    /// construction.
    #[must_use]
    pub fn wire_messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .flat_map(|entry| entry_wire_messages(entry))
            .collect()
    }

    /// Snapshot request positions beside their persisted source identities.
    /// Only the positions change when compaction moves retained entries.
    pub(crate) fn wire_messages_with_image_occurrences(
        &self,
    ) -> (Vec<Message>, HashMap<(usize, usize), String>) {
        let mut messages = Vec::new();
        let mut occurrences = HashMap::new();
        for entry in &self.entries {
            for (entry_message, message) in entry_wire_messages(entry).into_iter().enumerate() {
                let message_index = messages.len();
                for (block_index, block) in message.content.blocks.iter().enumerate() {
                    if matches!(block, crate::content::ContentBlock::Image { .. }) {
                        occurrences.insert(
                            (message_index, block_index),
                            format!("{}:{entry_message}:{block_index}", entry.id),
                        );
                    }
                }
                messages.push(message);
            }
        }
        (messages, occurrences)
    }

    /// Enumerate active provenance without cloning model-facing message text.
    pub(crate) fn image_observation_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        for entry in &self.entries {
            let mut add = |message: usize, content: &Content| {
                for (block, value) in content.blocks.iter().enumerate() {
                    if matches!(value, crate::content::ContentBlock::Image { .. }) {
                        ids.insert(format!("{}:{message}:{block}", entry.id));
                    }
                }
            };
            match &entry.kind {
                EntryKind::User { content } => add(0, content),
                EntryKind::Assistant { exchanges, .. } => {
                    for (index, exchange) in exchanges.iter().enumerate() {
                        if let Some(result) = &exchange.result {
                            add(index + 1, &result.content);
                        }
                    }
                }
                _ => {}
            }
        }
        ids
    }
}

const COMPACTION_IMAGE_SOURCE_CHARS: usize = 2_000;
const COMPACTION_IMAGE_SOURCE_COUNT: usize = 16;

fn compaction_image_source_text(sources: &[crate::session_entry::CompactionImageSource]) -> String {
    let mut text =
        "\n\n[归档图片来源数据；像素未提供 / Archived image source data; pixels not supplied]\n"
            .to_string();
    let mut shown = 0usize;
    // Leave room for the explicit metadata/omission notice below.
    const NOTICE_RESERVE: usize = 256;
    for source in sources.iter().rev().take(COMPACTION_IMAGE_SOURCE_COUNT) {
        let remaining =
            COMPACTION_IMAGE_SOURCE_CHARS.saturating_sub(text.chars().count() + NOTICE_RESERVE);
        if remaining < 128 {
            break;
        }
        let line = source.summary_text();
        text.push_str(&crate::text_util::truncate_chars(
            &line,
            remaining.min(512).saturating_sub(1),
        ));
        text.push('\n');
        shown += 1;
    }
    text.push_str(&format!("[Showing {shown} of {} archived image sources; full source metadata is retained in the session record. Source lines may be shortened.]", sources.len()));
    text
}

/// Wire messages for one entry. Shared with compaction's archived-entry
/// summarizer so summaries stay byte-equivalent with the message-based path.
pub(crate) fn entry_wire_messages(entry: &SessionEntry) -> Vec<Message> {
    match &entry.kind {
        EntryKind::System { content } => vec![Message::system(content.clone())],
        EntryKind::User { content } => vec![Message::user_content(
            content.map_images(|image| ImageRef::Asset(image.clone())),
        )],
        EntryKind::Compaction {
            summary,
            image_sources,
            ..
        } => {
            let mut content = format!("{COMPACTION_SUMMARY_PREFIX}{summary}");
            if !image_sources.is_empty() {
                content.push_str(&compaction_image_source_text(image_sources));
            }
            vec![Message::system(content)]
        }
        EntryKind::Assistant {
            content,
            reasoning,
            exchanges,
        } => {
            let calls = exchanges
                .iter()
                .map(|exchange| exchange.call.clone())
                .collect::<Vec<_>>();
            let mut out = Vec::with_capacity(1 + exchanges.len());
            out.push(Message::assistant_turn(
                content.clone(),
                reasoning.clone().unwrap_or_default(),
                calls,
            ));
            for exchange in exchanges {
                let content = exchange.result.as_ref().map_or_else(
                    || Content::from(INTERRUPTED_TOOL_RESULT),
                    |result| result.content.clone(),
                );
                out.push(Message::tool_content(
                    exchange.call.id.clone(),
                    content.map_images(|image| ImageRef::Asset(image.clone())),
                ));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolCallFunctionPayload, ToolCallPayload};

    #[test]
    fn image_occurrences_keep_source_identity_through_compaction_and_reload() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let image = crate::image::snapshot_bytes(&workspace, b"GIF89a-observation").unwrap();
        let mut session = Session::new();
        session.push_system("system");
        for _ in 0..6 {
            let turn = TurnId::new();
            session.push_user_with_images(turn.clone(), "observe", vec![image.clone()]);
            session.push_assistant(turn, "observed", "", Vec::new());
        }
        let (messages, before) = session.wire_messages_with_image_occurrences();
        assert_eq!(
            session.image_observation_ids(),
            before.values().cloned().collect()
        );
        let last_position = messages
            .iter()
            .rposition(|message| message.content.has_images())
            .unwrap();
        let identity = before[&(last_position, 1)].clone();
        let compacted = crate::compaction::compact_entries(session.entries());
        assert!(compacted.archived_count > 0);
        session.replace_entries(compacted.entries);
        let (messages, after) = session.wire_messages_with_image_occurrences();
        let new_position = messages
            .iter()
            .rposition(|message| message.content.has_images())
            .unwrap();
        assert!(new_position < last_position);
        assert_eq!(after[&(new_position, 1)], identity);
        let encoded = serde_json::to_vec(session.entries()).unwrap();
        let decoded = serde_json::from_slice(&encoded).unwrap();
        let restored = Session::from_entries(decoded);
        assert_eq!(
            restored.wire_messages_with_image_occurrences(),
            (messages, after)
        );
        let mut restored = restored;
        let same_turn = restored.entries().last().unwrap().turn_id.clone().unwrap();
        restored.push_user_with_images(same_turn, "observe again", vec![image]);
        let (_, occurrences) = restored.wire_messages_with_image_occurrences();
        assert_eq!(
            occurrences.values().filter(|id| **id == identity).count(),
            1
        );
        assert_eq!(
            occurrences
                .values()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            occurrences.len()
        );
    }

    #[test]
    fn late_partial_reply_cannot_reappear_after_its_anchor_was_compacted() {
        let mut session = Session::new();
        session.push_user(TurnId::new(), "old task");
        let anchor = session.entries().last().unwrap().clone();
        session.replace_entries(vec![Arc::new(SessionEntry::compaction("summary", 1))]);
        session.push_user(TurnId::new(), "new task");
        let before = session.wire_messages();
        assert!(!session.insert_partial_assistant_after(
            &anchor,
            TurnId::new(),
            "late".into(),
            "reason".into()
        ));
        assert_eq!(session.wire_messages(), before);
    }

    #[test]
    fn late_partial_reply_keeps_its_original_turn_identity() {
        let mut session = Session::new();
        let first = TurnId::new();
        let second = TurnId::new();
        session.push_user(first.clone(), "old task");
        let anchor = session.entries().last().unwrap().clone();
        session.push_user(second.clone(), "new task");
        assert!(session.insert_partial_assistant_after(
            &anchor,
            first.clone(),
            "late".into(),
            "reason".into()
        ));
        assert_eq!(session.entries()[1].turn_id.as_ref(), Some(&first));
        assert_eq!(session.entries()[2].turn_id.as_ref(), Some(&second));
    }

    fn tool_call(id: &str) -> ToolCallPayload {
        ToolCallPayload {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunctionPayload {
                name: "write_file".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    /// Every string this crate puts on the wire for the MODEL carries both
    /// languages — the summary marker, the compaction role labels, the
    /// cancellation note. This one is the placeholder a resumed-after-interrupt
    /// session hands back when the external outcome is unknown.
    #[test]
    fn the_interrupted_placeholder_is_bilingual() {
        assert!(INTERRUPTED_TOOL_RESULT.contains("工具执行结果未知"));
        assert!(
            INTERRUPTED_TOOL_RESULT.contains("Tool outcome unknown"),
            "{INTERRUPTED_TOOL_RESULT}"
        );
        // The line continuation must not leave the join inside the sentence.
        assert!(!INTERRUPTED_TOOL_RESULT.contains("  "));
    }

    #[test]
    fn golden_wire_equivalence_with_hand_built_messages() {
        // Build a representative transcript through the domain API and assert
        // the derived wire equals the hand-built message vector — this is
        // what keeps the DeepSeek prefix-cache fingerprint stable across the
        // refactor.
        let mut session = Session::new();
        let turn_id = TurnId::new();
        session.push_system("you are deep-code");
        session.push_user(turn_id.clone(), "修个 bug");
        session.push_assistant(
            turn_id.clone(),
            "看看文件",
            "先读再改",
            vec![tool_call("c1"), tool_call("c2")],
        );
        assert!(session.record_tool_result(
            "c1",
            "content-1".into(),
            ToolResultStatus::Success,
            Ask::Unasked,
            None,
        ));
        assert!(session.record_tool_result(
            "c2",
            "content-2".into(),
            ToolResultStatus::Error,
            Ask::Unasked,
            None,
        ));
        session.push_assistant(turn_id, "改好了", "", Vec::new());
        session.push_user(TurnId::new(), "thanks");

        let expected = vec![
            Message::system("you are deep-code"),
            Message::user("修个 bug"),
            Message::assistant_turn(
                "看看文件",
                "先读再改",
                vec![tool_call("c1"), tool_call("c2")],
            ),
            Message::tool("c1", "content-1"),
            Message::tool("c2", "content-2"),
            Message::assistant_turn("改好了", "", Vec::new()),
            Message::user("thanks"),
        ];
        assert_eq!(session.wire_messages(), expected);
    }

    #[test]
    fn record_tool_result_targets_latest_assistant_entry() {
        let mut session = Session::new();
        let turn_id = TurnId::new();
        session.push_assistant(turn_id.clone(), "first", "", vec![tool_call("c1")]);
        session.push_assistant(turn_id, "second", "", vec![tool_call("c9")]);

        // Unknown id → false; id from an OLDER entry → also false (results
        // always belong to the newest batch).
        assert!(!session.record_tool_result(
            "nope",
            Content::default(),
            ToolResultStatus::Success,
            Ask::Unasked,
            None,
        ));
        assert!(!session.record_tool_result(
            "c1",
            Content::default(),
            ToolResultStatus::Success,
            Ask::Unasked,
            None,
        ));
        assert!(session.record_tool_result(
            "c9",
            "ok".into(),
            ToolResultStatus::Success,
            Ask::Unasked,
            None,
        ));
        // Double-record of the same call is rejected.
        assert!(!session.record_tool_result(
            "c9",
            "again".into(),
            ToolResultStatus::Success,
            Ask::Unasked,
            None,
        ));
    }

    #[test]
    fn tool_observations_keep_role_order_and_metadata_out_of_model_content() {
        use crate::content::ContentBlock;
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let asset = crate::image::snapshot_bytes(&workspace, b"GIF89a-observed").unwrap();
        let turn = TurnId::new();
        let mut session = Session::new();
        session.push_user(turn.clone(), "inspect the page");
        session.push_assistant(turn.clone(), "", "", vec![tool_call("shot")]);
        let content = Content::new(vec![
            ContentBlock::Text {
                text: "before".into(),
            },
            ContentBlock::Image {
                image: asset.clone(),
            },
            ContentBlock::Text {
                text: "after".into(),
            },
        ]);
        assert!(session.record_tool_result(
            "shot",
            content,
            ToolResultStatus::Success,
            Ask::Unasked,
            Some(serde_json::json!({"exit_code":0, "ui_only":"secret"}))
        ));
        let messages = session.wire_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, crate::message::Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("shot"));
        assert!(
            matches!(&messages[2].content.blocks[0], ContentBlock::Text {text} if text == "before")
        );
        assert!(
            matches!(&messages[2].content.blocks[1], ContentBlock::Image {image:ImageRef::Asset(image)} if image == &asset)
        );
        assert!(
            matches!(&messages[2].content.blocks[2], ContentBlock::Text {text} if text == "after")
        );
        assert!(!messages[2].content.summary_text().contains("secret"));
        assert!(session.turn_has_images(&turn));
        let next = TurnId::new();
        session.push_user(next.clone(), "plain follow-up");
        assert!(!session.turn_has_images(&next));
    }

    #[test]
    fn compaction_keeps_full_source_metadata_but_bounds_its_wire_projection() {
        use crate::session_entry::CompactionImageSource;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let image = crate::image::snapshot_bytes(&workspace, b"GIF89a-bounded-sources").unwrap();
        let sources: Vec<_> = (0..100)
            .map(|index| CompactionImageSource {
                image: image.clone(),
                role: crate::message::Role::Tool,
                call_id: Some(format!("call-{index}")),
                tool_name: Some("long-tool-name".repeat(1_000)),
            })
            .collect();
        let entry = SessionEntry::new(EntryKind::Compaction {
            summary: "known visual conclusion".into(),
            archived_count: 200,
            image_sources: sources.clone(),
        });
        let wire = entry_wire_messages(&entry);
        let text = wire[0].content.text();
        assert!(
            text.chars().count()
                <= COMPACTION_SUMMARY_PREFIX.chars().count()
                    + "known visual conclusion".len()
                    + COMPACTION_IMAGE_SOURCE_CHARS
        );
        assert!(text.contains("call-99"));
        assert!(text.contains("of 100 archived image sources"));
        assert!(!wire[0].content.has_images());
        assert!(
            matches!(&entry.kind, EntryKind::Compaction {image_sources,..} if image_sources == &sources)
        );
    }
}
