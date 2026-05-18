//! deep-code agent core library.

mod client;
mod config;
mod error;
mod event;
mod message;
mod model;
mod session;

pub use client::{AgentEventStream, DeepSeekClient, LlmClient};
pub use config::{AgentConfig, DEFAULT_DEEPSEEK_BASE_URL, DEFAULT_DEEPSEEK_MODEL};
pub use error::{AgentError, AgentResult};
pub use event::{AgentEvent, chunk_to_events};
pub use message::{Message, Role};
pub use model::{
    ChatChoice, ChatRequest, ChoiceDelta, FunctionCallDelta, StreamChunk, ToolCallDelta, Usage,
};
pub use session::Session;
