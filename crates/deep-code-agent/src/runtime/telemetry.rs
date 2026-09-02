use serde::{Deserialize, Serialize};

use crate::compaction::{context_usage_percent, effective_compaction_threshold};
use crate::model::Usage;
use crate::model_registry::context_window_for_model;
use crate::model_route::TurnRoute;
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

/// One turn's telemetry snapshot: routing, token/cache counts, cost, and
/// context-pressure readouts. Built by `AgentRuntime::build_turn_telemetry`
/// and surfaced on `RuntimeEvent::TurnFinished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnTelemetry {
    pub route_label: String,
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
    #[serde(default)]
    pub route_reason: String,
    /// What decided the route this turn: `hard-rule` / `heuristic` / `cascade`.
    #[serde(default)]
    pub route_source: String,
    /// True on the turn where cascade escalation latched on (Flash's repeated
    /// tool-call failures crossed the threshold). That turn still ran on Flash;
    /// escalation takes effect from the next turn.
    #[serde(default)]
    pub cascade_triggered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    pub context_window: u32,
    pub estimated_context_tokens: u32,
    pub context_usage_percent: u8,
    pub near_compaction_threshold: bool,
    pub used_model_fallback: bool,
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
        route: &TurnRoute,
        usage: Option<&Usage>,
        prefix_hash: u64,
        estimated_context_tokens: u32,
        stream_retries: u32,
    ) -> TurnTelemetry {
        // `usage` is the turn's FINAL request: right for context-shaped
        // fields (prompt_tokens, context%), while costs/cache totals read the
        // per-request accumulators filled by `accumulate_request_usage`.
        let usage = usage.cloned().unwrap_or_default();
        let (
            prior_hash,
            turn_cost,
            turn_cache_hit_tokens,
            turn_cache_miss_tokens,
            session_cost,
            session_cache_hit_tokens,
            session_cache_miss_tokens,
            session_cache_savings,
            cascade_triggered,
        ) = {
            let mut state = self.state.lock().await;
            let prior_hash = state.last_prefix_hash;
            state.last_prefix_hash = Some(prefix_hash);
            (
                prior_hash,
                state.turn_cost,
                state.turn_cache_hit_tokens,
                state.turn_cache_miss_tokens,
                state.session_cost,
                u32::try_from(state.session_cache_hit_tokens).unwrap_or(u32::MAX),
                u32::try_from(state.session_cache_miss_tokens).unwrap_or(u32::MAX),
                state.session_cache_savings,
                state.cascade_triggered_this_turn,
            )
        };
        let prefix_status = match prior_hash {
            None => PrefixStatus::FirstTurn,
            Some(previous) if previous == prefix_hash => PrefixStatus::Stable,
            Some(_) => PrefixStatus::Changed,
        };
        let estimated_context_tokens = usage.input_tokens().max(estimated_context_tokens);
        let context_window = context_window_for_model(&route.effective_model);
        let message_estimate = estimated_context_tokens;

        TurnTelemetry {
            route_label: route.label(),
            effective_model: route.effective_model.clone(),
            reasoning_effort: route.effective_effort.short_label().to_string(),
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
            prefix_status,
            route_reason: route.route_reason.clone(),
            route_source: route.source.label().to_string(),
            cascade_triggered,
            fallback_reason: route.fallback_reason.clone(),
            context_window,
            estimated_context_tokens,
            context_usage_percent: context_usage_percent(
                estimated_context_tokens,
                &route.effective_model,
            ),
            near_compaction_threshold: message_estimate
                >= effective_compaction_threshold(
                    &route.effective_model,
                    self.config.compaction_threshold,
                )
                .saturating_mul(80)
                    / 100,
            used_model_fallback: route.used_model_fallback,
            stream_retries,
            turn_cost,
            session_cost,
        }
    }
}
