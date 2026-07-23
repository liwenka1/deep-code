//! Provider stream acquisition and robustness guards.
//!
//! This module owns everything between "we want a model stream" and "the turn
//! loop consumes `AgentEvent`s": auto-mode fallback, open-phase retry with
//! exponential backoff, transparent re-open before any content arrived,
//! per-chunk stall timeout, a total stream deadline, and a cumulative size
//! guard. The turn loop only calls [`AgentRuntime::open_turn_stream`] and then
//! drives [`GuardedStream::next`].
//!
//! Pinned retry/fallback order:
//! 1. retriable API errors (429/5xx) at open time fall back Pro→Flash first
//!    (existing auto-mode behavior, keeps the user-visible 降级 reason);
//! 2. if the open still fails with a retriable/transport error, back off and
//!    retry on the (possibly downgraded) route model;
//! 3. mid-stream errors re-open the same model, but only while no content has
//!    been received — once content arrived, errors surface immediately so
//!    billed output is never silently duplicated.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::time::Instant;

use crate::auto_mode::{TurnRoute, api_fallback_model, clamp_effort_to_model};
use crate::client::{AgentEventStream, LlmClient};
use crate::error::{AgentError, AgentResult};
use crate::event::AgentEvent;
use crate::model::ChatRequest;
use crate::runtime::AgentRuntime;

const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(4);

/// A provider stream wrapped with the turn's robustness guards.
///
/// `next()` is an inherent method (not the `Stream` trait) so the turn loop
/// keeps its `stream.next().await` shape. The returned future is drop-safe:
/// the caller's select-on-cancel aborts chunk waits and retry backoffs alike.
pub(super) struct GuardedStream<C: LlmClient + 'static> {
    client: Arc<C>,
    request: ChatRequest,
    inner: AgentEventStream,
    chunk_timeout: Duration,
    total_timeout: Duration,
    deadline: Instant,
    max_bytes: u64,
    bytes_seen: u64,
    retries_left: u32,
    retries_used: u32,
    backoff: Duration,
    any_content: bool,
    finished: bool,
}

impl<C: LlmClient + 'static> GuardedStream<C> {
    pub(super) async fn next(&mut self) -> Option<AgentResult<AgentEvent>> {
        if self.finished {
            return None;
        }
        loop {
            let now = Instant::now();
            if now >= self.deadline {
                self.finished = true;
                return Some(Err(AgentError::StreamDeadlineExceeded {
                    seconds: self.total_timeout.as_secs(),
                }));
            }
            let until_deadline = self.deadline - now;
            let wait = self.chunk_timeout.min(until_deadline);

            match tokio::time::timeout(wait, self.inner.next()).await {
                Err(_elapsed) => {
                    self.finished = true;
                    // Classify by what actually expired: the total deadline
                    // wins ties so the message points at the right knob.
                    let error = if Instant::now() >= self.deadline {
                        AgentError::StreamDeadlineExceeded {
                            seconds: self.total_timeout.as_secs(),
                        }
                    } else {
                        AgentError::StreamStalled {
                            seconds: self.chunk_timeout.as_secs(),
                        }
                    };
                    return Some(Err(error));
                }
                Ok(None) => {
                    self.finished = true;
                    return None;
                }
                Ok(Some(Ok(event))) => {
                    self.any_content = true;
                    self.bytes_seen += event_content_len(&event) as u64;
                    if self.bytes_seen > self.max_bytes {
                        self.finished = true;
                        return Some(Err(AgentError::StreamOverflow {
                            limit_bytes: self.max_bytes,
                        }));
                    }
                    return Some(Ok(event));
                }
                Ok(Some(Err(error))) => {
                    if self.any_content {
                        self.finished = true;
                        return Some(Err(error));
                    }
                    match self.reopen(error).await {
                        Ok(()) => {}
                        Err(error) => {
                            self.finished = true;
                            return Some(Err(error));
                        }
                    }
                }
            }
        }
    }

    /// Total transparent retries used by this stream (open phase included).
    #[must_use]
    pub(super) fn retries_used(&self) -> u32 {
        self.retries_used
    }

    async fn reopen(&mut self, original: AgentError) -> AgentResult<()> {
        let mut last_error = original;
        while self.retries_left > 0 {
            self.retries_left -= 1;
            self.retries_used += 1;
            tokio::time::sleep(self.backoff).await;
            self.backoff = self.backoff.saturating_mul(2).min(MAX_BACKOFF);
            match self.client.stream_chat(self.request.clone()).await {
                Ok(stream) => {
                    self.inner = stream;
                    return Ok(());
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Open the model stream for one turn with all guards attached.
    ///
    /// Open-phase failures retry with backoff (after the auto-mode fallback
    /// had its chance); the returned [`GuardedStream`] continues retrying
    /// transparently until first content. The caller is expected to wrap this
    /// future in its select-on-cancel: dropping it aborts any pending backoff.
    pub(super) async fn open_turn_stream(
        &self,
        route: &mut TurnRoute,
        request: ChatRequest,
    ) -> AgentResult<GuardedStream<C>> {
        let mut retries_left = self.config.stream_max_retries;
        let mut retries_used = 0u32;
        let mut backoff = INITIAL_BACKOFF;

        let inner = loop {
            // Refresh the model on every attempt: a fallback in a prior
            // attempt mutates the route and must stick for retries.
            let mut attempt = request.clone();
            attempt.model = route.effective_model.clone();
            match self.stream_with_fallback(route, attempt).await {
                Ok(stream) => break stream,
                Err(error) if retries_left > 0 && open_error_retriable(&error) => {
                    retries_left -= 1;
                    retries_used += 1;
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                }
                Err(error) => return Err(error),
            }
        };

        // Mid-stream re-opens stay on the route's final model: the fallback
        // decision was already taken (and surfaced) at open time.
        let mut request = request;
        request.model = route.effective_model.clone();

        Ok(GuardedStream {
            client: Arc::clone(&self.client),
            request,
            inner,
            chunk_timeout: self.config.stream_chunk_timeout,
            total_timeout: self.config.stream_total_timeout,
            deadline: Instant::now() + self.config.stream_total_timeout,
            max_bytes: self.config.stream_max_bytes,
            bytes_seen: 0,
            retries_left,
            retries_used,
            backoff,
            any_content: false,
            finished: false,
        })
    }

    async fn stream_with_fallback(
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
                    // Flash rejects `max`; cap the retry's effort to what it accepts.
                    route.effective_effort =
                        clamp_effort_to_model(fallback, route.effective_effort);
                    route.fallback_reason = Some(
                        crate::tr(self.ui_lang(), crate::TextId::RouteFallbackProToFlash)
                            .to_string(),
                    );
                    let mut retry = request;
                    retry.model = fallback.to_string();
                    retry.reasoning_effort =
                        route.effective_effort.as_api_value().map(str::to_string);
                    self.client.stream_chat(retry).await
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn event_content_len(event: &AgentEvent) -> usize {
    match event {
        AgentEvent::TextDelta { text } | AgentEvent::ReasoningDelta { text } => text.len(),
        AgentEvent::ToolCallDelta { delta } => delta
            .function
            .as_ref()
            .and_then(|function| function.arguments.as_ref())
            .map_or(0, String::len),
        AgentEvent::Done { .. } | AgentEvent::Error { .. } => 0,
    }
}

fn api_error_retriable(error: &AgentError) -> bool {
    match error {
        AgentError::Api { status, .. } => matches!(status.as_u16(), 429 | 502 | 503 | 504),
        _ => false,
    }
}

fn open_error_retriable(error: &AgentError) -> bool {
    api_error_retriable(error)
        || matches!(
            error,
            AgentError::Http(_) | AgentError::RequestTimeout { .. }
        )
}

#[cfg(test)]
#[path = "api_retriable_tests.rs"]
mod api_retriable_tests;
