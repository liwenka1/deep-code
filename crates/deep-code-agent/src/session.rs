use std::collections::HashSet;

use crate::message::{Message, Role};

/// Synthetic result for a tool call that never got a response — the session was
/// interrupted between the assistant's `tool_calls` and the tool executing.
const INTERRUPTED_TOOL_RESULT: &str = "工具调用未完成：会话在执行前被中断。";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    messages: Vec<Message>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    #[must_use]
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// Ensure every assistant `tool_calls` message is followed by a `tool`
    /// message for each call id. Interrupting a turn (exit while awaiting
    /// approval, crash mid-execution) can persist an assistant message whose
    /// tool calls were never answered; resuming and sending that history to the
    /// API violates the OpenAI/DeepSeek contract ("an assistant message with
    /// 'tool_calls' must be followed by tool messages"). Close any gap with a
    /// synthetic result so the conversation can continue.
    pub fn repair_dangling_tool_calls(&mut self) {
        let original = std::mem::take(&mut self.messages);
        let mut out: Vec<Message> = Vec::with_capacity(original.len());
        let mut iter = original.into_iter().peekable();
        while let Some(message) = iter.next() {
            if message.role != Role::Assistant || message.tool_calls.is_empty() {
                out.push(message);
                continue;
            }
            let call_ids: Vec<String> = message
                .tool_calls
                .iter()
                .map(|call| call.id.clone())
                .collect();
            out.push(message);
            // Consume the tool responses that immediately follow.
            let mut answered = HashSet::new();
            while iter.peek().is_some_and(|next| next.role == Role::Tool) {
                let tool_message = iter.next().expect("peeked tool message");
                if let Some(id) = &tool_message.tool_call_id {
                    answered.insert(id.clone());
                }
                out.push(tool_message);
            }
            for id in call_ids {
                if !answered.contains(&id) {
                    out.push(Message::tool(id, INTERRUPTED_TOOL_RESULT));
                }
            }
        }
        self.messages = out;
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
    fn repair_closes_dangling_tool_call_before_next_user_turn() {
        let mut session = Session::from_messages(vec![
            Message::user("写个 README"),
            Message::assistant_with_tool_calls("", vec![tool_call("call_1")]),
            // interrupted here: no tool result was persisted
            Message::user("继续"),
        ]);
        session.repair_dangling_tool_calls();
        let messages = session.messages();
        assert_eq!(messages.len(), 4);
        // Synthetic tool result inserted right after the tool_calls message.
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].role, Role::User);
    }

    #[test]
    fn repair_covers_each_unanswered_parallel_call() {
        let mut session = Session::from_messages(vec![
            Message::assistant_with_tool_calls("", vec![tool_call("a"), tool_call("b")]),
            Message::tool("a", "done"),
            // "b" never answered
        ]);
        session.repair_dangling_tool_calls();
        let answered: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(answered, vec!["a", "b"]);
    }

    #[test]
    fn repair_leaves_paired_history_untouched() {
        let original = vec![
            Message::assistant_with_tool_calls("", vec![tool_call("call_1")]),
            Message::tool("call_1", "ok"),
            Message::user("thanks"),
        ];
        let mut session = Session::from_messages(original.clone());
        session.repair_dangling_tool_calls();
        assert_eq!(session.messages(), original.as_slice());
    }
}
