use crate::message::Message;

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
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}
