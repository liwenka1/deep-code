use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::content::Content;
use crate::image::{ImageAsset, ImageRef};
use crate::message::Message;
use crate::model::ToolCallPayload;
use crate::runtime::TurnId;
use crate::session_entry::{
    Ask, EntryKind, ExchangeResult, SessionEntry, ToolExchange, UnsentMessage,
};
use crate::session_store::{DRAFT_STORAGE_BUDGET_BYTES, MAX_RETAINED_DRAFTS, charge_json};
use crate::tool::ToolResultStatus;
use crate::tool::details::{PreparedPreview, PreviewRetention};

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
/// every entry's bytes. Tool results and expired detail previews are updated
/// through [`Arc::make_mut`], cloning only an affected entry when a saved
/// snapshot still references it.
#[derive(Debug, Clone, Default)]
pub struct Session {
    entries: Vec<Arc<SessionEntry>>,
    previews: PreviewRetention,
}

pub(crate) struct RequestMessages {
    pub messages: Vec<Message>,
    pub image_occurrences: HashMap<(usize, usize), String>,
    /// User messages for the active turn, protected during image preparation.
    pub submissions: Vec<usize>,
    pub has_pending: bool,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for Session {}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_entries(mut entries: Vec<Arc<SessionEntry>>) -> Self {
        let previews = PreviewRetention::from_entries(&mut entries);
        Self { entries, previews }
    }

    #[must_use]
    pub fn entries(&self) -> &[Arc<SessionEntry>] {
        &self.entries
    }

    pub fn replace_entries(&mut self, entries: Vec<Arc<SessionEntry>>) {
        self.entries = entries;
        self.previews = PreviewRetention::from_entries(&mut self.entries);
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

    pub(crate) fn push_pending_user(
        &mut self,
        turn_id: TurnId,
        content: Content,
        unsent: UnsentMessage,
    ) {
        let mut entry = SessionEntry::user(content).with_turn_id(turn_id);
        entry.unsent = Some(unsent);
        self.push_entry(entry);
    }

    pub(crate) fn update_pending_user(
        &mut self,
        turn_id: &TurnId,
        content: Content,
        unsent: UnsentMessage,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.pending_for(Some(turn_id)) && matches!(entry.kind, EntryKind::User { .. })
        }) else {
            return false;
        };
        let entry = Arc::make_mut(entry);
        entry.kind = EntryKind::User { content };
        entry.unsent = Some(unsent);
        true
    }

    pub(crate) fn settle_submissions(&mut self, turn_id: &TurnId, error: Option<&str>) -> bool {
        let mut settled = false;
        for entry in &mut self.entries {
            if entry.turn_id.as_ref() != Some(turn_id) {
                continue;
            }
            let Some(draft) = entry.unsent.as_ref() else {
                continue;
            };
            if draft.error.is_some() {
                settled |= error.is_some();
                continue;
            }
            let entry = Arc::make_mut(entry);
            if let Some(error) = error {
                entry.unsent.as_mut().expect("pending submission").error = Some(error.to_owned());
            } else {
                entry.unsent = None;
            }
            settled = true;
        }
        settled
    }

    /// Keep the newest drafts that fit. Call while closing a turn or loading
    /// an idle session, so pending input is never evicted before submission.
    /// Return discarded turn IDs that no remaining entries reference.
    pub(crate) fn limit_retained_drafts(&mut self) -> HashSet<TurnId> {
        let mut remaining = DRAFT_STORAGE_BUDGET_BYTES;
        let mut slots = MAX_RETAINED_DRAFTS;
        let mut removed_turns = HashSet::new();
        let previous_len = self.entries.len();
        self.entries.reverse();
        self.entries.retain(|entry| {
            if entry.unsent.is_none() {
                return true;
            }
            if slots > 0 && charge_json(entry, &mut remaining) {
                slots -= 1;
                return true;
            }
            removed_turns.extend(entry.turn_id.iter().cloned());
            false
        });
        self.entries.reverse();
        if self.entries.len() != previous_len {
            for entry in &self.entries {
                if let Some(turn_id) = &entry.turn_id {
                    removed_turns.remove(turn_id);
                }
            }
            self.previews = PreviewRetention::from_entries(&mut self.entries);
        }
        removed_turns
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
        let Some(index) = self.entries.iter().position(|entry| entry.id == anchor.id) else {
            return false;
        };
        let reply =
            SessionEntry::assistant(content, Some(reasoning), Vec::new()).with_turn_id(turn_id);
        self.entries.insert(index + 1, Arc::new(reply));
        self.previews.inserted_entry(index + 1);
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
        let prepared = details.as_ref().and_then(PreparedPreview::new);
        self.record_prepared_tool_result(call_id, content, status, ask, details, prepared)
    }

    /// The runtime prepares display costs and compact values on its blocking
    /// worker. Recording under the state lock only updates the retention queue.
    pub(crate) fn record_prepared_tool_result(
        &mut self,
        call_id: &str,
        content: Content,
        status: ToolResultStatus,
        ask: Ask,
        details: Option<serde_json::Value>,
        prepared: Option<PreparedPreview>,
    ) -> bool {
        // Find the newest assistant entry first, and only `make_mut` (which
        // clones the entry while the persistence record also holds it) once a
        // matching pending exchange is confirmed — a miss must stay copy-free.
        let latest_assistant = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Assistant { .. }));
        let Some(entry_index) = latest_assistant else {
            return false;
        };
        let entry = &mut self.entries[entry_index];
        let EntryKind::Assistant { exchanges, .. } = &entry.kind else {
            unreachable!("filtered to assistant entries above");
        };
        let Some(exchange_index) = exchanges
            .iter()
            .position(|exchange| exchange.call.id == call_id && exchange.result.is_none())
        else {
            return false;
        };
        let EntryKind::Assistant { exchanges, .. } = &mut Arc::make_mut(entry).kind else {
            unreachable!("make_mut preserves the entry kind");
        };
        exchanges[exchange_index].result = Some(ExchangeResult {
            content,
            details,
            status,
            asked: ask == Ask::Asked,
        });
        self.previews
            .record(&mut self.entries, entry_index, exchange_index, prepared);
        true
    }

    /// Derive the DeepSeek/OpenAI wire messages. Pending exchanges emit the
    /// interrupted placeholder, so the protocol invariant (assistant
    /// `tool_calls` followed by one `role=tool` message per id) holds by
    /// construction.
    #[must_use]
    pub fn wire_messages(&self) -> Vec<Message> {
        self.wire_messages_for(None)
    }

    pub(crate) fn wire_messages_for(&self, turn_id: Option<&TurnId>) -> Vec<Message> {
        self.entries
            .iter()
            .flat_map(|entry| wire_entry(entry, entry.pending_for(turn_id)))
            .collect()
    }

    /// Snapshot request positions beside their persisted source identities.
    /// Only the positions change when compaction moves retained entries.
    pub(crate) fn wire_messages_with_image_occurrences(
        &self,
        turn_id: Option<&TurnId>,
    ) -> RequestMessages {
        let mut messages = Vec::new();
        let mut image_occurrences = HashMap::new();
        let mut submissions = Vec::new();
        let mut has_pending = false;
        for entry in &self.entries {
            let pending = entry.pending_for(turn_id);
            if pending && matches!(entry.kind, EntryKind::User { .. }) {
                submissions.push(messages.len());
            }
            has_pending |= pending;
            for (entry_message, message) in wire_entry(entry, pending).into_iter().enumerate() {
                let message_index = messages.len();
                for (block_index, block) in message.content.blocks.iter().enumerate() {
                    if matches!(block, crate::content::ContentBlock::Image { .. }) {
                        image_occurrences.insert(
                            (message_index, block_index),
                            format!("{}:{entry_message}:{block_index}", entry.id),
                        );
                    }
                }
                messages.push(message);
            }
        }
        RequestMessages {
            messages,
            image_occurrences,
            submissions,
            has_pending,
        }
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

#[cfg(test)]
#[path = "session/retention_tests.rs"]
mod retention_tests;

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
    wire_entry(entry, false)
}

fn wire_entry(entry: &SessionEntry, include_pending: bool) -> Vec<Message> {
    if entry.unsent.is_some() && !include_pending {
        return Vec::new();
    }
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
    fn compaction_counts_visible_entries_and_the_active_pending_user() {
        let mut session = Session::new();
        session.push_system("system");
        let accepted = TurnId::new();
        session.push_user(accepted.clone(), "accepted");
        session.push_assistant(accepted, "answer", "", Vec::new());
        for _ in 0..5 {
            session.push_pending_user(
                TurnId::new(),
                Content::from("rejected"),
                UnsentMessage::default(),
            );
        }
        assert!(crate::compaction::plan_compaction(session.entries(), None).is_none());
        for _ in 0..3 {
            session.push_system("received context");
        }
        let pending = TurnId::new();
        session.push_pending_user(
            pending.clone(),
            Content::from("sending now"),
            UnsentMessage::default(),
        );
        assert!(crate::compaction::plan_compaction(session.entries(), None).is_none());
        let plan = crate::compaction::plan_compaction(session.entries(), Some(&pending)).unwrap();
        assert_eq!(plan.tail_start(), 2);
    }

    #[test]
    fn draft_budget_counts_serialized_text_and_diagnostics_together() {
        let mut session = Session::new();
        let discarded = TurnId::new();
        let retained = TurnId::new();
        let draft = "x".repeat((DRAFT_STORAGE_BUDGET_BYTES / 4) as usize);
        for turn_id in [&discarded, &retained] {
            session.push_pending_user(
                turn_id.clone(),
                Content::from(draft.clone()),
                UnsentMessage {
                    error: Some(draft.clone()),
                    ..UnsentMessage::default()
                },
            );
        }
        assert_eq!(session.limit_retained_drafts(), HashSet::from([discarded]));
        assert_eq!(session.entries().len(), 1);
        assert_eq!(session.entries()[0].turn_id.as_ref(), Some(&retained));
    }

    #[test]
    fn compaction_preserves_failed_drafts_outside_model_context() {
        let mut session = Session::new();
        session.push_system("system");
        session.push_pending_user(
            TurnId::new(),
            Content::from("rejected draft"),
            UnsentMessage {
                error: Some("refused".into()),
                ..UnsentMessage::default()
            },
        );
        for _ in 0..6 {
            let turn_id = TurnId::new();
            session.push_user(turn_id.clone(), "submitted");
            session.push_assistant(turn_id, "answer", "", Vec::new());
        }
        let compacted = crate::compaction::compact_entries(session.entries());
        assert!(compacted.archived_count > 0);
        assert!(!compacted.summary.contains("rejected draft"));
        session.replace_entries(compacted.entries);
        assert!(session.entries().iter().any(|entry| entry.unsent.is_some()));
        assert!(
            session
                .wire_messages()
                .iter()
                .all(|message| !message.content.text().contains("rejected draft"))
        );
    }

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
        let RequestMessages {
            messages,
            image_occurrences: before,
            ..
        } = session.wire_messages_with_image_occurrences(None);
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
        let RequestMessages {
            messages,
            image_occurrences: after,
            ..
        } = session.wire_messages_with_image_occurrences(None);
        let new_position = messages
            .iter()
            .rposition(|message| message.content.has_images())
            .unwrap();
        assert!(new_position < last_position);
        assert_eq!(after[&(new_position, 1)], identity);
        let encoded = serde_json::to_vec(session.entries()).unwrap();
        let decoded = serde_json::from_slice(&encoded).unwrap();
        let restored = Session::from_entries(decoded);
        let restored_projection = restored.wire_messages_with_image_occurrences(None);
        assert_eq!(restored_projection.messages, messages);
        assert_eq!(restored_projection.image_occurrences, after);
        let mut restored = restored;
        let same_turn = restored.entries().last().unwrap().turn_id.clone().unwrap();
        restored.push_user_with_images(same_turn, "observe again", vec![image]);
        let occurrences = restored
            .wire_messages_with_image_occurrences(None)
            .image_occurrences;
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
