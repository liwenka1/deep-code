use crate::message::{Message, Role};
use crate::model::ToolCallPayload;
use crate::session_entry::{EntryKind, ExchangeResult, SessionEntry, ToolExchange};
use crate::tool::ToolResultStatus;

/// Synthetic wire content for a tool call whose result never arrived — the
/// session was interrupted between the assistant's `tool_calls` and the tool
/// executing. Synthesized at wire derivation, never stored.
pub(crate) const INTERRUPTED_TOOL_RESULT: &str = "工具调用未完成：会话在执行前被中断。";

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    entries: Vec<SessionEntry>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_entries(entries: Vec<SessionEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn replace_entries(&mut self, entries: Vec<SessionEntry>) {
        self.entries = entries;
    }

    pub fn push_system(&mut self, content: impl Into<String>) {
        self.entries.push(SessionEntry::system(content));
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.entries.push(SessionEntry::user(content));
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
        self.entries.push(SessionEntry::assistant(
            content,
            (!reasoning.is_empty()).then_some(reasoning),
            exchanges,
        ));
    }

    /// Record a tool result into the newest assistant entry's matching
    /// pending exchange. Calls always belong to the latest assistant entry
    /// (the runtime pushes the entry immediately before executing its batch).
    /// Returns false when no pending exchange matches.
    pub fn record_tool_result(
        &mut self,
        call_id: &str,
        content: String,
        status: ToolResultStatus,
    ) -> bool {
        let latest_assistant =
            self.entries
                .iter_mut()
                .rev()
                .find_map(|entry| match &mut entry.kind {
                    EntryKind::Assistant { exchanges, .. } => Some(exchanges),
                    _ => None,
                });
        let Some(exchanges) = latest_assistant else {
            return false;
        };
        match exchanges
            .iter_mut()
            .find(|exchange| exchange.call.id == call_id && exchange.result.is_none())
        {
            Some(exchange) => {
                exchange.result = Some(ExchangeResult { content, status });
                true
            }
            None => false,
        }
    }

    /// Derive the DeepSeek/OpenAI wire messages. Pending exchanges emit the
    /// interrupted placeholder, so the protocol invariant (assistant
    /// `tool_calls` followed by one `role=tool` message per id) holds by
    /// construction.
    #[must_use]
    pub fn wire_messages(&self) -> Vec<Message> {
        self.entries.iter().flat_map(entry_wire_messages).collect()
    }

    /// Group wire messages back into entries — the inverse of
    /// [`Session::wire_messages`] for protocol-valid histories. Used to load
    /// schema-v1 records; unanswered `tool_calls` become pending exchanges
    /// (the old `repair_dangling_tool_calls` semantics, applied structurally).
    #[must_use]
    pub fn from_wire_messages(messages: &[Message]) -> Self {
        let mut entries: Vec<SessionEntry> = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {
                    if let Some(summary) = message.content.strip_prefix(COMPACTION_SUMMARY_PREFIX) {
                        entries.push(SessionEntry::compaction(summary.to_string(), 0));
                    } else {
                        entries.push(SessionEntry::system(message.content.clone()));
                    }
                }
                Role::User => entries.push(SessionEntry::user(message.content.clone())),
                Role::Assistant => {
                    let exchanges = message
                        .tool_calls
                        .iter()
                        .cloned()
                        .map(ToolExchange::pending)
                        .collect();
                    entries.push(SessionEntry::assistant(
                        message.content.clone(),
                        message.reasoning_content.clone(),
                        exchanges,
                    ));
                }
                Role::Tool => {
                    let Some(call_id) = message.tool_call_id.as_deref() else {
                        continue;
                    };
                    let slot = entries.iter_mut().rev().find_map(|entry| {
                        let EntryKind::Assistant { exchanges, .. } = &mut entry.kind else {
                            return None;
                        };
                        exchanges.iter_mut().find(|exchange| {
                            exchange.call.id == call_id && exchange.result.is_none()
                        })
                    });
                    if let Some(exchange) = slot {
                        // Wire messages carry no status; the schema-v2
                        // migration recovers it from TurnRecord. In-memory
                        // resume only needs the content for wire round-trips.
                        exchange.result = Some(ExchangeResult {
                            content: message.content.clone(),
                            status: ToolResultStatus::Success,
                        });
                    }
                    // Orphan tool messages (no matching call) were already
                    // protocol-invalid in v1; drop them.
                }
            }
        }
        Self { entries }
    }
}

/// Wire messages for one entry. Shared with compaction's archived-entry
/// summarizer so summaries stay byte-equivalent with the message-based path.
pub(crate) fn entry_wire_messages(entry: &SessionEntry) -> Vec<Message> {
    match &entry.kind {
        EntryKind::System { content } => vec![Message::system(content.clone())],
        EntryKind::User { content } => vec![Message::user(content.clone())],
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

    #[test]
    fn wire_synthesizes_placeholder_for_interrupted_exchange() {
        let session = Session::from_wire_messages(&[
            Message::user("写个 README"),
            Message::assistant_with_tool_calls("", vec![tool_call("call_1")]),
            // interrupted here: no tool result was persisted
            Message::user("继续"),
        ]);
        let messages = session.wire_messages();
        assert_eq!(messages.len(), 4);
        // Synthetic tool result derived right after the tool_calls message.
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[2].content, INTERRUPTED_TOOL_RESULT);
        assert_eq!(messages[3].role, Role::User);
    }

    #[test]
    fn wire_covers_each_unanswered_parallel_call() {
        let session = Session::from_wire_messages(&[
            Message::assistant_with_tool_calls("", vec![tool_call("a"), tool_call("b")]),
            Message::tool("a", "done"),
            // "b" never answered
        ]);
        let wire = session.wire_messages();
        let answered: Vec<(String, &str)> = wire
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| {
                (
                    message.tool_call_id.clone().unwrap(),
                    if message.content == "done" {
                        "done"
                    } else {
                        "synth"
                    },
                )
            })
            .collect();
        assert_eq!(
            answered,
            vec![("a".to_string(), "done"), ("b".to_string(), "synth")]
        );
    }

    #[test]
    fn wire_round_trips_paired_history() {
        let original = vec![
            Message::assistant_with_tool_calls("", vec![tool_call("call_1")]),
            Message::tool("call_1", "ok"),
            Message::user("thanks"),
        ];
        let session = Session::from_wire_messages(&original);
        assert_eq!(session.wire_messages(), original);
    }

    #[test]
    fn golden_wire_equivalence_with_v1_construction() {
        // Build a representative transcript through the domain API and assert
        // the derived wire equals the hand-built v1 message vector — this is
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
            ToolResultStatus::Success
        ));
        assert!(session.record_tool_result("c2", "content-2".to_string(), ToolResultStatus::Error));
        session.push_assistant("改好了", "", Vec::new());
        session.push_user("thanks");

        let v1 = vec![
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
        assert_eq!(session.wire_messages(), v1);

        // And the grouping inverse reproduces the same wire.
        assert_eq!(Session::from_wire_messages(&v1).wire_messages(), v1);
    }

    #[test]
    fn record_tool_result_targets_latest_assistant_entry() {
        let mut session = Session::new();
        session.push_assistant("first", "", vec![tool_call("c1")]);
        session.push_assistant("second", "", vec![tool_call("c9")]);

        // Unknown id → false; id from an OLDER entry → also false (results
        // always belong to the newest batch).
        assert!(!session.record_tool_result("nope", String::new(), ToolResultStatus::Success));
        assert!(!session.record_tool_result("c1", String::new(), ToolResultStatus::Success));
        assert!(session.record_tool_result("c9", "ok".to_string(), ToolResultStatus::Success));
        // Double-record of the same call is rejected.
        assert!(!session.record_tool_result("c9", "again".to_string(), ToolResultStatus::Success));
    }

    #[test]
    fn compaction_summary_round_trips_through_grouping() {
        let mut session = Session::new();
        session.replace_entries(vec![
            SessionEntry::system("sys"),
            SessionEntry::compaction("- 用户: 旧对话", 12),
            SessionEntry::user("new question"),
        ]);
        let wire = session.wire_messages();
        assert!(wire[1].content.starts_with("[会话摘要"));

        let regrouped = Session::from_wire_messages(&wire);
        assert!(matches!(
            &regrouped.entries()[1].kind,
            EntryKind::Compaction { summary, .. } if summary == "- 用户: 旧对话"
        ));
        assert_eq!(regrouped.wire_messages(), wire);
    }
}
