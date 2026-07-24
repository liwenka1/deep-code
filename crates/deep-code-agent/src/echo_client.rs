//! Offline placeholder backend implementing [`LlmClient`].
//!
//! Used when `DEEPSEEK_API_KEY` is missing: the TUI still starts (so a
//! first-run user can type `/apikey` to connect), but every submission
//! returns a fixed setup hint instead of pretending to work.

use async_stream::try_stream;
use async_trait::async_trait;

use crate::client::{AgentEventStream, LlmClient};
use crate::error::AgentResult;
use crate::event::AgentEvent;
use crate::i18n::{SharedLang, TextId, tr};
use crate::model::ChatRequest;

#[derive(Debug, Clone)]
pub struct EchoClient {
    /// UI language for the offline hint reply (our own copy, not model output).
    /// A `SharedLang` shared with the runtime, so `/lang` live-switches the hint
    /// without a relaunch.
    lang: SharedLang,
}

impl EchoClient {
    pub const MODEL: &'static str = "echo-offline";
    pub const PROVIDER: &'static str = "echo";

    #[must_use]
    pub fn new(lang: SharedLang) -> Self {
        Self { lang }
    }
}

#[async_trait]
impl LlmClient for EchoClient {
    fn provider_name(&self) -> &'static str {
        Self::PROVIDER
    }

    fn model(&self) -> &str {
        Self::MODEL
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
        let hint = tr(self.lang.get(), TextId::EchoOfflineHint).to_string();
        let stream = try_stream! {
            yield AgentEvent::TextDelta { text: hint };
            yield AgentEvent::Done { usage: None };
        };
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use futures_util::StreamExt;

    async fn first_text(client: &EchoClient) -> String {
        let mut stream = client
            .stream_chat(ChatRequest::streaming(EchoClient::MODEL, Vec::new()))
            .await
            .unwrap();
        while let Some(Ok(event)) = stream.next().await {
            if let AgentEvent::TextDelta { text } = event {
                return text;
            }
        }
        String::new()
    }

    #[tokio::test]
    async fn offline_hint_localizes() {
        assert!(
            first_text(&EchoClient::new(SharedLang::new(Lang::Zh)))
                .await
                .contains("接入")
        );
        assert!(
            first_text(&EchoClient::new(SharedLang::new(Lang::En)))
                .await
                .contains("API key")
        );
    }

    #[tokio::test]
    async fn offline_hint_follows_shared_lang() {
        // A `/lang` flip (via the shared atomic) must change the hint without
        // rebuilding the client.
        let lang = SharedLang::new(Lang::En);
        let client = EchoClient::new(lang.clone());
        assert!(first_text(&client).await.contains("API key"));
        lang.set(Lang::Zh);
        assert!(first_text(&client).await.contains("接入"));
    }
}
