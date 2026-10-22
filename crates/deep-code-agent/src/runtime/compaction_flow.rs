use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    CompactionPlan, HANDOVER_CUT_SHORT_NOTE, HANDOVER_SYSTEM_PROMPT, MAX_HANDOVER_SUMMARY_CHARS,
    apply_compaction, handover_request, mechanical_summary, plan_compaction, should_compact,
};
use crate::echo_client::EchoClient;
use crate::event::AgentEvent;
use crate::message::Message;
use crate::model::ChatRequest;
use crate::reasoning::ReasoningEffort;
use crate::runtime::event::{RuntimeEvent, TurnId, emit};
use crate::runtime::{AgentRuntime, CompactNowOutcome, CompactionReport};
use crate::session_entry::SessionEntry;
use crate::text_util::truncate_chars;

/// Wall-clock bound on the handover request, retries included. Generous — the
/// summarizer reads up to a few hundred thousand characters — but finite: a
/// provider that accepts the connection and goes silent must not park a
/// compaction (and the turn behind it) forever. What running out costs depends
/// on who asked; see [`Trigger`].
const HANDOVER_DEADLINE: Duration = Duration::from_secs(180);

/// Room for the longest summary the prompt allows (about 3000 CJK characters)
/// and for the reasoning the model does before it: DeepSeek counts reasoning
/// inside the completion, so a cap sized for the text alone cut long summaries
/// off mid-section. The output is also capped by chars after the fact.
const HANDOVER_MAX_TOKENS: u32 = 12_000;

/// What asked for a compaction. They differ in what may happen when the model
/// cannot deliver a summary — and none of them folds anything once the cancel
/// token has fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    /// `/compact`. A failure leaves the history as it was and says why; the
    /// user can run it again.
    Manual,
    /// The context crossed `compaction_threshold` before a model step. The
    /// request still fits the window, so the fold is optional: a failure skips
    /// it for the rest of the turn instead of writing the lossy kind.
    Threshold,
    /// The provider refused the request as over its window. Folding is the only
    /// way the turn can go on, so the mechanical summary stands in when the
    /// model cannot write one.
    OverflowRescue,
}

/// One applied compaction.
pub(super) struct Folded {
    pub archived_count: usize,
    pub summary: String,
    /// The model's summary ran into a length cap; the text ends with
    /// [`HANDOVER_CUT_SHORT_NOTE`].
    pub cut_short: bool,
}

/// How a compaction attempt ended. Every answer but `Applied` left the history
/// untouched.
pub(super) enum FoldOutcome {
    Applied(Folded),
    /// Nothing to archive.
    Nothing,
    /// No model to write the summary (the offline backend).
    Offline,
    /// The summarizer could not deliver and the trigger may not fall back.
    Failed(String),
    /// The cancel token fired first.
    Cancelled,
}

/// Why the summarizer did not deliver.
enum HandoverError {
    Offline,
    Cancelled,
    Failed(String),
}

/// The summarizer's text, and whether a length cap cut it short.
struct Handover {
    text: String,
    cut_short: bool,
}

impl AgentRuntime {
    /// The threshold compaction before a model step: when the estimate says
    /// the context crossed `compaction_threshold`, and at most once per turn.
    ///
    /// Once per turn because a fold never reaches the turn's own entries (the
    /// newest user message anchors the kept tail): after the first, all that
    /// is left to archive is the previous turn's tail, one entry per step as
    /// the window slides, and each of those folds was a summarizer round trip
    /// of its own. A failed attempt counts too — the next step would only fail
    /// the same way, and the overflow rescue is still there if the context
    /// really runs out.
    ///
    /// `cancel` is the turn's token: an Esc while the summarizer writes ends
    /// the turn with nothing folded.
    pub(super) async fn maybe_compact(
        &self,
        model: &str,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
        cancel: &CancellationToken,
        turn_id: &TurnId,
    ) {
        let wire = {
            let state = self.state.lock().await;
            if state.threshold_compaction_tried.as_ref() == Some(turn_id) {
                return;
            }
            state.session.wire_messages()
        };
        if !should_compact(model, &wire, self.config.compaction_threshold) {
            return;
        }
        let outcome = self
            .compact_session(Trigger::Threshold, Some(tx), None, cancel)
            .await;
        if !matches!(outcome, FoldOutcome::Nothing) {
            self.state.lock().await.threshold_compaction_tried = Some(turn_id.clone());
        }
    }

    /// Compact now, threshold or not, because the user ran `/compact` — they
    /// often know something the estimate does not (that the last twenty turns
    /// were a dead end, or that a big task is about to start). `focus` is their
    /// own steer for the summary (`/compact keep the sandbox decisions`), if
    /// they gave one; `cancel` is theirs to fire.
    ///
    /// Only a summary the model wrote is ever applied here. Every other answer
    /// leaves the history exactly as it was — see [`CompactNowOutcome`].
    pub async fn compact_now(
        &self,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> CompactNowOutcome {
        // Measured around the fold, and only here: the automatic path fires on
        // a threshold (so "did it buy anything" is not in question) and its
        // event carries no numbers to report them with.
        let tokens_before = self.context_tokens().await;
        match self
            .compact_session(Trigger::Manual, None, focus, cancel)
            .await
        {
            FoldOutcome::Applied(folded) => CompactNowOutcome::Folded(CompactionReport {
                archived_entries: folded.archived_count,
                summary: folded.summary,
                tokens_before,
                tokens_after: self.context_tokens().await,
                cut_short: folded.cut_short,
            }),
            FoldOutcome::Nothing => CompactNowOutcome::NothingToFold,
            FoldOutcome::Offline => CompactNowOutcome::Offline,
            FoldOutcome::Failed(reason) => CompactNowOutcome::Failed(reason),
            FoldOutcome::Cancelled => CompactNowOutcome::Cancelled,
        }
    }

    /// Estimated tokens in the model-visible context right now.
    async fn context_tokens(&self) -> u32 {
        let wire = self.state.lock().await.session.wire_messages();
        crate::compaction::estimate_token_count(&wire)
    }

    /// Fold history: ask the model for a handover summary of the archived
    /// range and replace the range with it. When the model cannot deliver, only
    /// an overflow rescue folds anyway (mechanically); see [`Trigger`].
    ///
    /// `tx` is `None` for a compaction no turn is running (`/compact`): its
    /// events have no turn stream to ride, so the caller renders the outcome
    /// itself rather than events nobody is listening for. With a stream, the
    /// start of the summarizer's wait, a fallback and a skipped fold are all
    /// announced on it.
    pub(super) async fn compact_session(
        &self,
        trigger: Trigger,
        tx: Option<&mpsc::UnboundedSender<RuntimeEvent>>,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> FoldOutcome {
        let entries: Vec<Arc<SessionEntry>> = {
            let state = self.state.lock().await;
            state.session.entries().to_vec()
        };
        let Some(plan) = plan_compaction(&entries) else {
            return FoldOutcome::Nothing;
        };
        // The offline backend answers every request with its setup hint, and
        // taken for a summary that hint replaced the archived history.
        let handover = if self.client.provider_name() == EchoClient::PROVIDER {
            Err(HandoverError::Offline)
        } else {
            if let Some(tx) = tx {
                emit(tx, RuntimeEvent::CompactionStarted);
            }
            self.handover_summary(&plan, &entries, focus, cancel).await
        };
        let (summary, cut_short, mut fallback_reason) = match handover {
            Ok(Handover { text, cut_short }) => (text, cut_short, None),
            // The turn is ending, or the user changed their mind: either way
            // nothing is folded, and a later compaction has the originals.
            Err(HandoverError::Cancelled) => return FoldOutcome::Cancelled,
            Err(error) if trigger == Trigger::OverflowRescue => {
                let reason = match error {
                    HandoverError::Failed(reason) => reason,
                    HandoverError::Offline | HandoverError::Cancelled => "offline".to_string(),
                };
                (mechanical_summary(&plan, &entries), false, Some(reason))
            }
            Err(HandoverError::Offline) => return FoldOutcome::Offline,
            Err(HandoverError::Failed(reason)) => {
                self.announce_skipped_fold(tx, &reason);
                return FoldOutcome::Failed(reason);
            }
        };

        // A cancel that landed while the summary was finishing still means
        // "stop": nothing is folded once it has fired, whatever came back.
        if cancel.is_cancelled() {
            return FoldOutcome::Cancelled;
        }
        let (result, compacted) = {
            let mut state = self.state.lock().await;
            let current = state.session.entries().to_vec();
            // The summary describes the snapshot's archived range. Inside this
            // runtime's own turn loop nothing rewrites the session while a fold
            // runs, but a hand-run compaction runs beside whatever else its
            // front-end allows, and a turn may have appended meanwhile. Appends
            // are harmless: the plan's indices still name the same entries and
            // the new ones land in the kept tail. Anything else — another fold,
            // a swapped session — and the summary is about a range that no
            // longer exists as such.
            let kept_from = plan.tail_start();
            let prefix_intact = current.len() >= kept_from
                && current[..kept_from]
                    .iter()
                    .zip(&entries[..kept_from])
                    .all(|(now, then)| Arc::ptr_eq(now, then));
            let result = if prefix_intact {
                apply_compaction(&current, &plan, summary)
            } else if trigger == Trigger::OverflowRescue {
                let Some(plan) = plan_compaction(&current) else {
                    return FoldOutcome::Nothing;
                };
                fallback_reason =
                    Some("the transcript changed while the summary was being written".to_string());
                let summary = mechanical_summary(&plan, &current);
                apply_compaction(&current, &plan, summary)
            } else {
                drop(state);
                let reason = "the history changed while the summary was being written".to_string();
                self.announce_skipped_fold(tx, &reason);
                return FoldOutcome::Failed(reason);
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
        let cut_short = cut_short && fallback_reason.is_none();
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
            if cut_short {
                emit(
                    tx,
                    RuntimeEvent::Warning {
                        message: crate::tr(self.ui_lang(), crate::TextId::CompactionCutShort)
                            .to_string(),
                    },
                );
            }
            emit(
                tx,
                RuntimeEvent::CompactionApplied {
                    archived_count: result.archived_count,
                    summary: result.summary.clone(),
                    model_written: fallback_reason.is_none(),
                },
            );
        }
        FoldOutcome::Applied(Folded {
            archived_count: result.archived_count,
            summary: result.summary,
            cut_short,
        })
    }

    /// Tell a running turn that its threshold fold did not happen, and why.
    /// The history is as it was, and the turn goes on with it.
    fn announce_skipped_fold(
        &self,
        tx: Option<&mpsc::UnboundedSender<RuntimeEvent>>,
        reason: &str,
    ) {
        if let Some(tx) = tx {
            emit(
                tx,
                RuntimeEvent::Warning {
                    message: crate::tr_with(
                        self.ui_lang(),
                        crate::TextId::CompactionSkipped,
                        &[("reason", reason)],
                    ),
                },
            );
        }
    }

    /// Ask the model for the handover summary of `plan`'s archived range.
    ///
    /// A separate, tool-less request on the flash tier (the same choice the
    /// auto-mode judge makes, and for the same reason: the input is large and
    /// the task is reading, not deciding); low reasoning effort, since the
    /// value is in the transcript, not in deliberation. Bounded by
    /// [`HANDOVER_DEADLINE`] and by `cancel`, which is the caller's — the
    /// turn's for a fold inside a turn, the user's own for `/compact`. Never
    /// the runtime's turn token from between turns: that one stays cancelled
    /// after an Esc until the next turn begins.
    async fn handover_summary(
        &self,
        plan: &CompactionPlan,
        entries: &[Arc<SessionEntry>],
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Handover, HandoverError> {
        let model = super::approval_flow::classifier_model_for(&self.config, &self.registry);
        let mut request = ChatRequest::streaming(
            model.clone(),
            vec![
                Message::system(HANDOVER_SYSTEM_PROMPT),
                Message::user(handover_request(
                    plan,
                    entries,
                    focus,
                    self.compact_instructions.as_deref(),
                )),
            ],
        );
        request.max_tokens = Some(HANDOVER_MAX_TOKENS);
        if let Some(effort) = ReasoningEffort::Low.as_api_value() {
            request = request.with_reasoning_effort(effort);
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(HandoverError::Cancelled),
            outcome = tokio::time::timeout(HANDOVER_DEADLINE, self.collect_handover(&model, request)) => {
                outcome
                    .unwrap_or_else(|_| {
                        Err(format!("no reply within {}s", HANDOVER_DEADLINE.as_secs()))
                    })
                    .map_err(HandoverError::Failed)
            }
        }
    }

    /// Run the summarizer request to completion and return its text, trimmed
    /// and capped at [`MAX_HANDOVER_SUMMARY_CHARS`]; billed to the session like
    /// any other request on the key. Opened with the turn requests' retry and
    /// guards (`open_side_stream`), so a transient 429 or 503 is retried rather
    /// than costing the fold.
    async fn collect_handover(
        &self,
        model: &str,
        request: ChatRequest,
    ) -> Result<Handover, String> {
        let mut stream = self
            .open_side_stream(request)
            .await
            .map_err(|error| error.to_string())?;
        let mut text = String::new();
        let mut hit_token_cap = false;
        let mut finished = false;
        while let Some(event) = stream.next().await {
            match event {
                Ok(AgentEvent::TextDelta { text: delta }) => text.push_str(&delta),
                Ok(AgentEvent::Error { message }) => return Err(message),
                Err(error) => return Err(error.to_string()),
                Ok(AgentEvent::Done { usage }) => {
                    finished = true;
                    if let Some(usage) = usage.as_ref() {
                        self.accumulate_request_usage(model, usage).await;
                        // The client does not surface `finish_reason`, but a
                        // completion that used the whole allowance is one the
                        // provider stopped, not one the model finished.
                        hit_token_cap = usage
                            .completion_tokens
                            .is_some_and(|tokens| tokens >= HANDOVER_MAX_TOKENS);
                    }
                    break;
                }
                Ok(_) => {}
            }
        }
        // A stream that stopped without the provider's closing chunk ended
        // mid-reply, however clean the end looked: whatever arrived is a
        // fragment, not a summary.
        if !finished {
            return Err("the reply ended before the model finished".to_string());
        }
        let text = text.trim();
        if text.is_empty() {
            return Err("empty reply".to_string());
        }
        let cut_short = hit_token_cap || text.chars().count() > MAX_HANDOVER_SUMMARY_CHARS;
        if !cut_short {
            return Ok(Handover {
                text: text.to_string(),
                cut_short,
            });
        }
        // The note goes inside the cap, not past it: the next fold carries
        // the summary clipped to that cap, and the note it would cut off is
        // the one telling the reader the summary is incomplete.
        let room = MAX_HANDOVER_SUMMARY_CHARS - HANDOVER_CUT_SHORT_NOTE.chars().count() - 3;
        let mut text = truncate_chars(text, room);
        text.push_str("\n\n");
        text.push_str(HANDOVER_CUT_SHORT_NOTE);
        Ok(Handover { text, cut_short })
    }
}
