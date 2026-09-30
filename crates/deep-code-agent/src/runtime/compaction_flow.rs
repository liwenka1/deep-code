use std::sync::Arc;

use tokio::sync::mpsc;

use crate::compaction::{compact_entries, should_compact};
use crate::runtime::AgentRuntime;
use crate::runtime::CompactionReport;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::session_entry::SessionEntry;

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
        self.compact_session(Some(tx)).await.is_some()
    }

    /// Compact now, threshold or not — for a caller that knows something the
    /// estimate does not (a provider that already refused the request, or a user
    /// who ran `/compact`).
    ///
    /// `None` means there was nothing to archive: the transcript is already
    /// short. That is a normal answer to a hand-issued `/compact`, and a dead
    /// end for the rescue path — so it is reported rather than swallowed.
    pub async fn compact_now(&self) -> Option<CompactionReport> {
        // Measured around the fold, and only here: the automatic path fires on
        // a threshold (so "did it buy anything" is not in question) and its
        // event carries no numbers to report them with.
        let tokens_before = self.context_tokens().await;
        let (archived_entries, summary) = self.compact_session(None).await?;
        let tokens_after = self.context_tokens().await;
        Some(CompactionReport {
            archived_entries,
            summary,
            tokens_before,
            tokens_after,
        })
    }

    /// Estimated tokens in the model-visible context right now.
    async fn context_tokens(&self) -> u32 {
        let wire = self.state.lock().await.session.wire_messages();
        crate::compaction::estimate_token_count(&wire)
    }

    /// Fold history and report what happened as `(archived_entries, summary)`,
    /// or `None` when nothing could be archived.
    ///
    /// `tx` is `None` for a compaction no turn is running (`/compact`): its
    /// event has no turn stream to ride, so the caller renders the result
    /// itself rather than a `CompactionApplied` nobody is listening for.
    pub(super) async fn compact_session(
        &self,
        tx: Option<&mpsc::UnboundedSender<RuntimeEvent>>,
    ) -> Option<(usize, String)> {
        let entries: Vec<Arc<SessionEntry>> = {
            let state = self.state.lock().await;
            state.session.entries().to_vec()
        };
        let result = compact_entries(&entries);
        if result.archived_count == 0 {
            return None;
        }
        let compacted = {
            let mut state = self.state.lock().await;
            state.session.replace_entries(result.entries.clone());
            // No prefix reset here. Compaction replaces the history with a
            // shorter, different one, so `telemetry::probe_prefix` reports
            // `Changed` on its own — which is also the accurate label, where
            // the reset used to make the turn after a compaction read
            // `FirstTurn`.
            state.session.entries().to_vec()
        };
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
            emit(
                tx,
                RuntimeEvent::CompactionApplied {
                    archived_count: result.archived_count,
                    summary: result.summary.clone(),
                },
            );
        }
        Some((result.archived_count, result.summary))
    }
}
