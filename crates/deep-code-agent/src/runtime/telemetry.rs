use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::compaction::{context_usage_percent, effective_compaction_threshold};
use crate::message::Message;
use crate::model::Usage;
use crate::model_registry::context_window_for_model;
use crate::pricing::{CostEstimate, calculate_turn_cost};
use crate::runtime::AgentRuntime;

/// Prompt-prefix cache status for a turn. Presentation (the user-facing tag)
/// lives in the TUI's language pack; this stays a plain data enum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefixStatus {
    FirstTurn,
    Stable,
    Changed,
}

/// What this request's wire messages say about the prompt prefix the previous
/// turn left in the provider's cache, and what the next turn compares against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PrefixProbe {
    pub(super) status: PrefixStatus,
    /// This request's own `(message count, fingerprint)`, carried into state so
    /// the next turn can ask whether it is still a prefix.
    carried: (usize, u64),
}

/// Fingerprint an exact run of wire messages.
///
/// Every field that reaches the provider, because the question this answers is
/// "are these the same bytes": role, content, the reasoning replay, the tool
/// call id, and each tool call's id/name/arguments. Hashing `role:content`
/// alone (as this used to) gave two histories differing only in their tool
/// calls the same fingerprint.
fn fingerprint(messages: &[Message]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for message in messages {
        message.role.as_str().hash(&mut hasher);
        message.content.hash(&mut hasher);
        message.reasoning_content.hash(&mut hasher);
        message.tool_call_id.hash(&mut hasher);
        for call in &message.tool_calls {
            call.id.hash(&mut hasher);
            call.function.name.hash(&mut hasher);
            call.function.arguments.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Whether `messages` still opens with the exact run the previous turn sent.
///
/// The comparison is anchored on the previous request's LENGTH, and that is the
/// whole of the fix. The old fingerprint hashed `messages[..len - 1]` of
/// whatever it was handed and compared those across turns — but a turn appends
/// to the transcript, so turn N hashed `[sys, u1]` and turn N+1 hashed
/// `[sys, u1, a1, u2]`, which can never be equal. `Stable` was therefore
/// unreachable after the first turn and `/status` read "prefix changed" on
/// every single turn, including the ideal append-only case the rest of this
/// crate works hard to preserve (`appending_entries_preserves_wire_prefix`
/// asserts exactly that invariant). Remembering how many messages went out is
/// what turns "did it change?" into a question with a true answer.
pub(super) fn probe_prefix(messages: &[Message], prior: Option<(usize, u64)>) -> PrefixProbe {
    let status = match prior {
        None => PrefixStatus::FirstTurn,
        Some((len, hash)) if messages.len() >= len && fingerprint(&messages[..len]) == hash => {
            PrefixStatus::Stable
        }
        // Includes every real prefix rewrite: a compaction (which replaces the
        // history with a shorter, different one) reports `Changed` on its own,
        // with no separate reset to remember to perform.
        Some(_) => PrefixStatus::Changed,
    };
    PrefixProbe {
        status,
        carried: (messages.len(), fingerprint(messages)),
    }
}

/// One turn's telemetry snapshot: model settings, token/cache counts, cost, and
/// context-pressure readouts. Built by `AgentRuntime::build_turn_telemetry`
/// and surfaced on `RuntimeEvent::TurnFinished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnTelemetry {
    pub effective_model: String,
    pub reasoning_effort: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_hit_tokens: Option<u32>,
    pub cache_miss_tokens: Option<u32>,
    /// Cumulative session cache tokens, for a session-wide hit-rate readout.
    #[serde(default)]
    pub session_cache_hit_tokens: u32,
    #[serde(default)]
    pub session_cache_miss_tokens: u32,
    /// Cumulative spend avoided by cache hits this session (vs all-miss).
    #[serde(default)]
    pub session_cache_savings: CostEstimate,
    pub prefix_status: PrefixStatus,
    pub context_window: u32,
    pub estimated_context_tokens: u32,
    pub context_usage_percent: u8,
    pub near_compaction_threshold: bool,
    /// Transparent stream retries used this turn (0 when the network is fine).
    #[serde(default)]
    pub stream_retries: u32,
    pub turn_cost: CostEstimate,
    pub session_cost: CostEstimate,
}

impl AgentRuntime {
    /// Accumulate one completed request's usage into the turn- and
    /// session-level totals. Called at every stream `Done`: a multi-tool turn
    /// makes several requests, and pricing only the final one (or none, when
    /// the turn is cancelled mid-way) under-counts real spend.
    pub(super) async fn accumulate_request_usage(&self, model: &str, usage: &Usage) {
        let request_cost = calculate_turn_cost(model, usage);
        let cache_hit = usage.prompt_cache_hit_tokens.unwrap_or(0);
        let cache_miss = usage.prompt_cache_miss_tokens.unwrap_or(0);
        let savings = crate::pricing::cache_savings(model, cache_hit);
        let mut state = self.state.lock().await;
        state.turn_cost.usd += request_cost.usd;
        state.turn_cost.cny += request_cost.cny;
        state.turn_cache_hit_tokens += u64::from(cache_hit);
        state.turn_cache_miss_tokens += u64::from(cache_miss);
        state.session_cost.usd += request_cost.usd;
        state.session_cost.cny += request_cost.cny;
        state.session_cache_hit_tokens += u64::from(cache_hit);
        state.session_cache_miss_tokens += u64::from(cache_miss);
        state.session_cache_savings.usd += savings.usd;
        state.session_cache_savings.cny += savings.cny;
    }

    pub(super) async fn build_turn_telemetry(
        &self,
        model: &str,
        usage: Option<&Usage>,
        prefix: PrefixProbe,
        estimated_context_tokens: u32,
        stream_retries: u32,
    ) -> TurnTelemetry {
        // `usage` is the turn's FINAL request: right for context-shaped
        // fields (prompt_tokens, context%), while costs/cache totals read the
        // per-request accumulators filled by `accumulate_request_usage`.
        let usage = usage.cloned().unwrap_or_default();
        let (
            turn_cost,
            turn_cache_hit_tokens,
            turn_cache_miss_tokens,
            session_cost,
            session_cache_hit_tokens,
            session_cache_miss_tokens,
            session_cache_savings,
        ) = {
            let mut state = self.state.lock().await;
            state.last_prefix = Some(prefix.carried);
            (
                state.turn_cost,
                state.turn_cache_hit_tokens,
                state.turn_cache_miss_tokens,
                state.session_cost,
                u32::try_from(state.session_cache_hit_tokens).unwrap_or(u32::MAX),
                u32::try_from(state.session_cache_miss_tokens).unwrap_or(u32::MAX),
                state.session_cache_savings,
            )
        };
        let estimated_context_tokens = usage.input_tokens().max(estimated_context_tokens);
        let context_window = context_window_for_model(model);

        TurnTelemetry {
            effective_model: model.to_string(),
            reasoning_effort: self.config.reasoning_effort.as_setting().to_string(),
            prompt_tokens: usage.input_tokens(),
            completion_tokens: usage.output_tokens(),
            // Whole-turn cache totals (accumulated per request); fall back to
            // the final request's fields when nothing was accumulated so a
            // provider that omits cache usage still reads as "not reported".
            cache_hit_tokens: (turn_cache_hit_tokens > 0)
                .then(|| u32::try_from(turn_cache_hit_tokens).unwrap_or(u32::MAX))
                .or(usage.prompt_cache_hit_tokens),
            cache_miss_tokens: (turn_cache_miss_tokens > 0)
                .then(|| u32::try_from(turn_cache_miss_tokens).unwrap_or(u32::MAX))
                .or(usage.prompt_cache_miss_tokens),
            session_cache_hit_tokens,
            session_cache_miss_tokens,
            session_cache_savings,
            prefix_status: prefix.status,
            context_window,
            estimated_context_tokens,
            context_usage_percent: context_usage_percent(estimated_context_tokens, model),
            near_compaction_threshold: estimated_context_tokens
                >= effective_compaction_threshold(model, self.config.compaction_threshold)
                    .saturating_mul(80)
                    / 100,
            stream_retries,
            turn_cost,
            session_cost,
        }
    }
}
