use crate::checkpoint::CheckpointId;
use crate::client::LlmClient;
use crate::i18n::Lang;
use crate::message::Message;
use crate::runtime::AgentRuntime;
use crate::runtime::event::RuntimeEventReceiver;
use crate::session_store::SessionId;
use crate::tool::{ApprovalDecision, ToolError};

/// Object-safe handle so that callers (UIs, tests) can hold heterogeneous
/// runtimes (DeepSeek, offline echo, scripted, ...) behind a `Box<dyn ...>`.
///
/// Methods here return owned futures so the trait stays object-safe even
/// though [`LlmClient`] is not.
pub trait AgentRuntimeHandle: Send + Sync {
    fn submit_user(
        &self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>;

    fn submit_approval(
        &self,
        decision: ApprovalDecision,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>;

    /// Cancel the in-flight turn, if any. See [`AgentRuntime::cancel_turn`].
    fn cancel_turn(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>;

    fn session_messages(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Message>> + Send + '_>>;

    fn restore_checkpoint(
        &self,
        id: CheckpointId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>;

    fn session_id(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SessionId>> + Send + '_>>;

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;

    /// Update the runtime's UI language live (no relaunch). See
    /// [`AgentRuntime::set_ui_lang`].
    fn set_ui_lang(&self, lang: Lang);
}

impl<C: LlmClient + 'static> AgentRuntimeHandle for AgentRuntime<C> {
    fn submit_user(
        &self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>
    {
        Box::pin(AgentRuntime::submit_user(self, prompt))
    }

    fn submit_approval(
        &self,
        decision: ApprovalDecision,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>
    {
        Box::pin(AgentRuntime::submit_approval(self, decision))
    }

    fn cancel_turn(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeEventReceiver> + Send + '_>>
    {
        Box::pin(AgentRuntime::cancel_turn(self))
    }

    fn session_messages(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Message>> + Send + '_>> {
        Box::pin(AgentRuntime::session_messages(self))
    }

    fn restore_checkpoint(
        &self,
        id: CheckpointId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>
    {
        Box::pin(AgentRuntime::restore_checkpoint(self, id))
    }

    fn session_id(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SessionId>> + Send + '_>> {
        Box::pin(AgentRuntime::session_id(self))
    }

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(AgentRuntime::shutdown(self))
    }

    fn set_ui_lang(&self, lang: Lang) {
        AgentRuntime::set_ui_lang(self, lang);
    }
}
