use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::compaction::{
    CompactionPlan, HANDOVER_SYSTEM_PROMPT, MAX_HANDOVER_SUMMARY_CHARS, apply_compaction,
    handover_request, mechanical_summary, plan_compaction, should_compact,
};
use crate::event::AgentEvent;
use crate::message::Message;
use crate::model::ChatRequest;
use crate::reasoning::ReasoningEffort;
use crate::runtime::AgentRuntime;
use crate::runtime::CompactionReport;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::session_entry::SessionEntry;
use crate::text_util::truncate_chars;

/// Wall-clock bound on the handover request. Generous — the summarizer reads
/// up to a few hundred thousand characters — but finite: a provider that
/// accepts the connection and goes silent must not park a compaction (and the
/// turn behind it) forever. On expiry the fold falls back to the mechanical
/// summary, so nothing is lost but the quality of the summary.
const HANDOVER_DEADLINE: Duration = Duration::from_secs(180);

/// Room for the longest summary the prompt allows (about 3000 CJK characters)
/// with margin; the output is also capped by chars after the fact.
const HANDOVER_MAX_TOKENS: u32 = 6_000;

/// One applied compaction, as the two callers consume it.
pub(super) struct Folded {
    pub archived_count: usize,
    pub summary: String,
    /// The summary is the model's handover; `false` means the mechanical fold
    /// stood in for it, and `fallback_reason` says why.
    pub model_written: bool,
    pub fallback_reason: Option<String>,
}

impl AgentRuntime {
    /// Compact when the threshold says so, or unconditionally when `force`.
    pub(super) async fn maybe_compact(
        &self,
        model: &str,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        force: bool,
    ) -> bool {
        if !force {
            let wire = self.state.lock().await.session.wire_messages();
            if !should_compact(model, &wire, self.config.compaction_threshold) {
                return false;
            }
        }
        self.compact_session(Some(tx), None).await.is_some()
    }

    /// Compact now, threshold or not — for a caller that knows something the
    /// estimate does not (a provider that already refused the request, or a user
    /// who ran `/compact`). `focus` is the user's own steer for the summary
    /// (`/compact keep the sandbox decisions`), if they gave one.
    ///
    /// `None` means there was nothing to archive: the transcript is already
    /// short. That is a normal answer to a hand-issued `/compact`, and a dead
    /// end for the rescue path — so it is reported rather than swallowed.
    pub async fn compact_now(&self, focus: Option<&str>) -> Option<CompactionReport> {
        // Measured around the fold, and only here: the automatic path fires on
        // a threshold (so "did it buy anything" is not in question) and its
        // event carries no numbers to report them with.
        let tokens_before = self.context_tokens().await;
        let folded = self.compact_session(None, focus).await?;
        let tokens_after = self.context_tokens().await;
        Some(CompactionReport {
            archived_entries: folded.archived_count,
            summary: folded.summary,
            tokens_before,
            tokens_after,
            model_written: folded.model_written,
            fallback_reason: folded.fallback_reason,
        })
    }

    /// Estimated tokens in the model-visible context right now.
    async fn context_tokens(&self) -> u32 {
        let wire = self.state.lock().await.session.wire_messages();
        crate::compaction::estimate_token_count(&wire)
    }

    /// Fold history: ask the model for a handover summary of the archived
    /// range, fall back to the mechanical fold when it cannot deliver one, and
    /// replace the range with the result. `None` when nothing could be
    /// archived.
    ///
    /// `tx` is `None` for a compaction no turn is running (`/compact`): its
    /// event has no turn stream to ride, so the caller renders the result
    /// itself rather than a `CompactionApplied` nobody is listening for. With a
    /// stream, a fallback also raises a `Warning` so the user learns that this
    /// summary is the lesser kind.
    pub(super) async fn compact_session(
        &self,
        tx: Option<&mpsc::UnboundedSender<RuntimeEvent>>,
        focus: Option<&str>,
    ) -> Option<Folded> {
        let entries: Vec<Arc<SessionEntry>> = {
            let state = self.state.lock().await;
            state.session.entries().to_vec()
        };
        let plan = plan_compaction(&entries)?;
        let (summary, model_written, mut fallback_reason) =
            match self.handover_summary(&plan, &entries, focus).await {
                Ok(summary) => (summary, true, None),
                Err(reason) => (mechanical_summary(&plan, &entries), false, Some(reason)),
            };

        let (result, compacted) = {
            let mut state = self.state.lock().await;
            let current = state.session.entries().to_vec();
            // The summary describes the snapshot's archived range. Nothing
            // appends to the session while a compaction runs — the turn loop is
            // the only writer during a turn and it is the caller here; `/compact`
            // runs between turns — but applying a plan by index to a list that
            // moved anyway would archive the wrong entries, so the list is
            // checked rather than assumed. If it moved, the fold is re-planned
            // on what is there now, with the mechanical summary (the model's
            // text was about a range that no longer exists as such).
            let unchanged = current.len() == entries.len()
                && current
                    .iter()
                    .zip(&entries)
                    .all(|(now, then)| Arc::ptr_eq(now, then));
            let result = if unchanged {
                apply_compaction(&current, &plan, summary)
            } else {
                let plan = plan_compaction(&current)?;
                fallback_reason =
                    Some("the transcript changed while the summary was being written".to_string());
                let summary = mechanical_summary(&plan, &current);
                apply_compaction(&current, &plan, summary)
            };
            state.session.replace_entries(result.entries.clone());
            // No prefix reset here. Compaction replaces the history with a
            // shorter, different one, so `telemetry::probe_prefix` reports
            // `Changed` on its own — which is also the accurate label, where
            // the reset used to make the turn after a compaction read
            // `FirstTurn`.
            let compacted = state.session.entries().to_vec();
            (result, compacted)
        };
        let model_written = model_written && fallback_reason.is_none();
        if let Some(persistence) = self.persistence.as_ref() {
            {
                let mut record = persistence.record.lock().await;
                record.entries = compacted;
                record.summary = Some(result.summary.clone());
                record.compaction = Some(format!("archived={}", result.archived_count));
                record.touch();
            }
            persistence.actor.request_save();
        }
        if let Some(tx) = tx {
            if let Some(reason) = &fallback_reason {
                emit(
                    tx,
                    RuntimeEvent::Warning {
                        message: crate::tr_with(
                            self.ui_lang(),
                            crate::TextId::CompactionFallback,
                            &[("reason", reason)],
                        ),
                    },
                );
            }
            emit(
                tx,
                RuntimeEvent::CompactionApplied {
                    archived_count: result.archived_count,
                    summary: result.summary.clone(),
                    model_written,
                },
            );
        }
        Some(Folded {
            archived_count: result.archived_count,
            summary: result.summary,
            model_written,
            fallback_reason,
        })
    }

    /// Ask the model for the handover summary of `plan`'s archived range.
    ///
    /// A separate, tool-less request on the flash tier (the same choice the
    /// auto-mode judge makes, and for the same reason: the input is large and
    /// the task is reading, not deciding); low reasoning effort, since the
    /// value is in the transcript, not in deliberation. Bounded by
    /// [`HANDOVER_DEADLINE`] and by the turn's cancel token — an Esc during an
    /// automatic compaction must not wait on the summarizer. The reply is
    /// trimmed and capped at [`MAX_HANDOVER_SUMMARY_CHARS`]. `Err` carries the
    /// human-readable reason the caller reports alongside its fallback.
    async fn handover_summary(
        &self,
        plan: &CompactionPlan,
        entries: &[Arc<SessionEntry>],
        focus: Option<&str>,
    ) -> Result<String, String> {
        let model = super::approval_flow::classifier_model_for(&self.config, &self.registry);
        let mut request = ChatRequest::streaming(
            model.clone(),
            vec![
                Message::system(HANDOVER_SYSTEM_PROMPT),
                Message::user(handover_request(plan, entries, focus)),
            ],
        );
        request.max_tokens = Some(HANDOVER_MAX_TOKENS);
        if let Some(effort) = ReasoningEffort::Low.as_api_value() {
            request = request.with_reasoning_effort(effort);
        }
        let cancel = self.state.lock().await.cancel.clone();
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err("cancelled".to_string()),
            outcome = tokio::time::timeout(HANDOVER_DEADLINE, self.collect_handover(&model, request)) => {
                outcome.unwrap_or_else(|_| {
                    Err(format!("no reply within {}s", HANDOVER_DEADLINE.as_secs()))
                })
            }
        }
    }

    /// Run the summarizer request to completion and return its text; billed
    /// to the session like any other request on the key.
    async fn collect_handover(&self, model: &str, request: ChatRequest) -> Result<String, String> {
        let mut stream = self
            .client
            .stream_chat(request)
            .await
            .map_err(|error| error.to_string())?;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(AgentEvent::TextDelta { text: delta }) => text.push_str(&delta),
                Ok(AgentEvent::Error { message }) => return Err(message),
                Err(error) => return Err(error.to_string()),
                Ok(AgentEvent::Done { usage }) => {
                    if let Some(usage) = usage.as_ref() {
                        self.accumulate_request_usage(model, usage).await;
                    }
                    break;
                }
                Ok(_) => {}
            }
        }
        let text = text.trim();
        if text.is_empty() {
            return Err("empty reply".to_string());
        }
        Ok(truncate_chars(text, MAX_HANDOVER_SUMMARY_CHARS))
    }
}
