use deep_code_agent::{
    AgentConfig, AgentEvent, AgentEventStream, ChatRequest, DeepSeekClient, LlmClient, Message,
    Session,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub author: Author,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamUpdate {
    Event(AgentEvent),
    Error(String),
    Finished,
}

pub type StreamReceiver = mpsc::UnboundedReceiver<StreamUpdate>;

#[derive(Debug)]
pub struct App {
    pub input: String,
    pub messages: Vec<ChatMessage>,
    pub streaming_buffer: String,
    pub status: String,
    pub error: Option<String>,
    pub should_quit: bool,
    pub is_streaming: bool,
    session: Session,
    stream_rx: Option<StreamReceiver>,
    use_real_agent: bool,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        let config = AgentConfig::from_env();
        let use_real_agent = config.api_key.is_some();
        let status = if use_real_agent {
            format!("Ready - DeepSeek {}", config.model)
        } else {
            "Ready - offline echo mode (set DEEPSEEK_API_KEY for DeepSeek)".to_string()
        };

        let mut session = Session::new();
        session.push(Message::system(
            "You are deep-code's minimal TUI assistant.",
        ));

        Self {
            input: String::new(),
            messages: vec![ChatMessage {
                author: Author::System,
                text: "Type a prompt and press Enter. Press Esc or Ctrl+C to exit.".to_string(),
            }],
            streaming_buffer: String::new(),
            status,
            error: None,
            should_quit: false,
            is_streaming: false,
            session,
            stream_rx: None,
            use_real_agent,
        }
    }

    pub fn push_char(&mut self, value: char) {
        if !self.is_streaming {
            self.input.push(value);
        }
    }

    pub fn backspace(&mut self) {
        if !self.is_streaming {
            self.input.pop();
        }
    }

    pub fn submit(&mut self) {
        if self.is_streaming {
            return;
        }

        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            self.status = "Enter a prompt before sending.".to_string();
            return;
        }

        self.input.clear();
        self.error = None;
        self.streaming_buffer.clear();
        self.is_streaming = true;
        self.status = if self.use_real_agent {
            "Streaming from DeepSeek...".to_string()
        } else {
            "Streaming offline echo...".to_string()
        };

        self.session.push(Message::user(prompt.clone()));
        self.messages.push(ChatMessage {
            author: Author::User,
            text: prompt.clone(),
        });

        let messages = self.session.messages().to_vec();
        let (tx, rx) = mpsc::unbounded_channel();
        self.stream_rx = Some(rx);

        if self.use_real_agent {
            tokio::spawn(stream_deepseek(messages, tx));
        } else {
            tokio::spawn(stream_echo(prompt, tx));
        }
    }

    pub fn drain_stream_updates(&mut self) {
        let Some(mut rx) = self.stream_rx.take() else {
            return;
        };

        while let Ok(update) = rx.try_recv() {
            self.apply_stream_update(update);
        }

        if self.is_streaming {
            self.stream_rx = Some(rx);
        }
    }

    fn apply_stream_update(&mut self, update: StreamUpdate) {
        match update {
            StreamUpdate::Event(AgentEvent::TextDelta { text })
            | StreamUpdate::Event(AgentEvent::ReasoningDelta { text }) => {
                self.streaming_buffer.push_str(&text);
            }
            StreamUpdate::Event(AgentEvent::ToolCallDelta { .. }) => {
                self.status =
                    "Received tool call delta; tools are not enabled in this MVP.".to_string();
            }
            StreamUpdate::Event(AgentEvent::Done { .. }) | StreamUpdate::Finished => {
                self.finish_stream();
            }
            StreamUpdate::Event(AgentEvent::Error { message }) | StreamUpdate::Error(message) => {
                self.error = Some(message.clone());
                self.status = "Agent error.".to_string();
                self.messages.push(ChatMessage {
                    author: Author::System,
                    text: format!("Error: {message}"),
                });
                self.is_streaming = false;
                self.stream_rx = None;
            }
        }
    }

    fn finish_stream(&mut self) {
        if !self.is_streaming {
            return;
        }

        let text = if self.streaming_buffer.is_empty() {
            "(empty response)".to_string()
        } else {
            std::mem::take(&mut self.streaming_buffer)
        };

        self.session.push(Message::assistant(text.clone()));
        self.messages.push(ChatMessage {
            author: Author::Assistant,
            text,
        });
        self.status = "Ready".to_string();
        self.is_streaming = false;
        self.stream_rx = None;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

async fn stream_echo(prompt: String, tx: mpsc::UnboundedSender<StreamUpdate>) {
    let response = format!("Echo: {prompt}");
    for token in response.split_inclusive(' ') {
        if tx
            .send(StreamUpdate::Event(AgentEvent::TextDelta {
                text: token.to_string(),
            }))
            .is_err()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
    }

    let _ = tx.send(StreamUpdate::Event(AgentEvent::Done { usage: None }));
}

async fn stream_deepseek(messages: Vec<Message>, tx: mpsc::UnboundedSender<StreamUpdate>) {
    match open_deepseek_stream(messages).await {
        Ok(mut stream) => {
            while let Some(event) = stream.next().await {
                let update = match event {
                    Ok(event) => StreamUpdate::Event(event),
                    Err(error) => StreamUpdate::Error(error.to_string()),
                };

                let is_error = matches!(update, StreamUpdate::Error(_));
                if tx.send(update).is_err() || is_error {
                    return;
                }
            }

            let _ = tx.send(StreamUpdate::Finished);
        }
        Err(error) => {
            let _ = tx.send(StreamUpdate::Error(error.to_string()));
        }
    }
}

async fn open_deepseek_stream(
    messages: Vec<Message>,
) -> deep_code_agent::AgentResult<AgentEventStream> {
    let config = AgentConfig::from_env();
    let client = DeepSeekClient::new(config.clone())?;
    let request = ChatRequest::streaming(config.model.clone(), messages);

    client.stream_chat(request).await
}
