use crate::auto_mode::{TurnRoute, api_fallback_model};
use crate::client::{AgentEventStream, LlmClient};
use crate::error::AgentError;
use crate::model::ChatRequest;
use crate::runtime::AgentRuntime;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub(super) async fn stream_with_fallback(
        &self,
        route: &mut TurnRoute,
        request: ChatRequest,
    ) -> Result<AgentEventStream, AgentError> {
        match self.client.stream_chat(request.clone()).await {
            Ok(stream) => Ok(stream),
            Err(error) if api_error_retriable(&error) => {
                if let Some(fallback) = api_fallback_model(route) {
                    route.effective_model = fallback.to_string();
                    route.used_model_fallback = true;
                    let mut retry = request;
                    retry.model = fallback.to_string();
                    self.client.stream_chat(retry).await
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn api_error_retriable(error: &AgentError) -> bool {
    match error {
        AgentError::Api { status, .. } => matches!(status.as_u16(), 429 | 502 | 503 | 504),
        _ => false,
    }
}

#[cfg(test)]
#[path = "api_retriable_tests.rs"]
mod api_retriable_tests;
