use std::path::PathBuf;
use std::sync::Arc;

use crate::image::ImageRef;
use crate::message::Message;
use crate::model::ToolCallPayload;
use crate::session_entry::{Ask, EntryKind, ExchangeResult, SessionEntry, ToolExchange};
use crate::tool::ToolResultStatus;

/// Synthetic wire content for a tool call whose result never arrived — the
/// session was interrupted between the assistant's `tool_calls` and the tool
/// executing. Synthesized at wire derivation, never stored.
///
/// Bilingual like every other model-facing string this crate writes
/// (`[会话摘要 / session summary]`, `工具 / tool`, `运行环境 / Environment`).
/// It was missed by the pass that fixed the compaction role labels, which is
/// easy to do — it is the one such string with no `tr` call and no language
/// pack entry anywhere near it, and it only ever appears in a session that was
/// interrupted.
pub(crate) const INTERRUPTED_TOOL_RESULT: &str = "工具调用未完成：会话在执行前被中断。/ Tool call did not complete: \
     the session was interrupted before it ran.";

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

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.push_user_with_images(content, Vec::new());
    }

    /// Append a user turn that carries images.
    ///
    /// Separate from `push_user` rather than folded into it: the images are
    /// local paths (see [`SessionEntry::user_with_images`]), and a call site
    /// that has none should not have to spell an empty `Vec` to say so.
    pub fn push_user_with_images(&mut self, content: impl Into<String>, images: Vec<PathBuf>) {
        self.push_entry(SessionEntry::user_with_images(content, images));
    }

    /// Append an assistant turn; `calls` become pending exchanges whose
    /// results are filled in by [`Session::record_tool_result`].
    pub fn push_assistant(
        &mut self,
        content: impl Into<String>,
        reasoning: impl Into<String>,
        calls: Vec<ToolCallPayload>,
    ) {
        let reasoning = reasoning.into();
        let exchanges = calls.into_iter().map(ToolExchange::pending).collect();
        self.push_entry(SessionEntry::assistant(
            content,
            (!reasoning.is_empty()).then_some(reasoning),
            exchanges,
        ));
    }

    /// Keep a late partial reply beside the request it answered. A newer turn
    /// may already have appended its user message. If compaction replaced the
    /// anchor, discard the fragment instead of attaching it to a different turn.
    pub(crate) fn insert_partial_assistant_after(
        &mut self,
        anchor: &Arc<SessionEntry>,
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
        self.entries.insert(
            index + 1,
            Arc::new(SessionEntry::assistant(
                content,
                Some(reasoning),
                Vec::new(),
            )),
        );
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
        content: String,
        status: ToolResultStatus,
        ask: Ask,
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
            status,
            asked: ask == Ask::Asked,
        });
        true
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
}

/// Wire messages for one entry. Shared with compaction's archived-entry
/// summarizer so summaries stay byte-equivalent with the message-based path.
pub(crate) fn entry_wire_messages(entry: &SessionEntry) -> Vec<Message> {
    match &entry.kind {
        EntryKind::System { content } => vec![Message::system(content.clone())],
        EntryKind::User { content, images } => vec![Message::user_with_images(
            content.clone(),
            images.iter().cloned().map(ImageRef::Path).collect(),
        )],
        EntryKind::Compaction { summary, .. } => vec![Message::system(format!(
            "{COMPACTION_SUMMARY_PREFIX}{summary}"
        ))],
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
                let content = exchange
                    .result
                    .as_ref()
                    .map_or(INTERRUPTED_TOOL_RESULT, |result| result.content.as_str());
                out.push(Message::tool(exchange.call.id.clone(), content));
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
    fn late_partial_reply_cannot_reappear_after_its_anchor_was_compacted() {
        let mut session = Session::new();
        session.push_user("old task");
        let anchor = session.entries().last().unwrap().clone();
        session.replace_entries(vec![Arc::new(SessionEntry::compaction("summary", 1))]);
        session.push_user("new task");
        let before = session.wire_messages();
        assert!(!session.insert_partial_assistant_after(&anchor, "late".into(), "reason".into()));
        assert_eq!(session.wire_messages(), before);
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
    /// session hands back for a tool call that never ran, and it was the last
    /// Chinese-only one left.
    #[test]
    fn the_interrupted_placeholder_is_bilingual() {
        assert!(INTERRUPTED_TOOL_RESULT.contains("工具调用未完成"));
        assert!(
            INTERRUPTED_TOOL_RESULT.contains("interrupted before it ran"),
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
        session.push_system("you are deep-code");
        session.push_user("修个 bug");
        session.push_assistant(
            "看看文件",
            "先读再改",
            vec![tool_call("c1"), tool_call("c2")],
        );
        assert!(session.record_tool_result(
            "c1",
            "content-1".to_string(),
            ToolResultStatus::Success,
            Ask::Unasked
        ));
        assert!(session.record_tool_result(
            "c2",
            "content-2".to_string(),
            ToolResultStatus::Error,
            Ask::Unasked
        ));
        session.push_assistant("改好了", "", Vec::new());
        session.push_user("thanks");

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
        session.push_assistant("first", "", vec![tool_call("c1")]);
        session.push_assistant("second", "", vec![tool_call("c9")]);

        // Unknown id → false; id from an OLDER entry → also false (results
        // always belong to the newest batch).
        assert!(!session.record_tool_result(
            "nope",
            String::new(),
            ToolResultStatus::Success,
            Ask::Unasked
        ));
        assert!(!session.record_tool_result(
            "c1",
            String::new(),
            ToolResultStatus::Success,
            Ask::Unasked
        ));
        assert!(session.record_tool_result(
            "c9",
            "ok".to_string(),
            ToolResultStatus::Success,
            Ask::Unasked
        ));
        // Double-record of the same call is rejected.
        assert!(!session.record_tool_result(
            "c9",
            "again".to_string(),
            ToolResultStatus::Success,
            Ask::Unasked
        ));
    }
}
