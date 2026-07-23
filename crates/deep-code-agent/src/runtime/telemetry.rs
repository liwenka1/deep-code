use crate::model_route::TurnRoute;
use crate::client::LlmClient;
use crate::compaction::{context_usage_percent, effective_compaction_threshold};
use crate::model::Usage;
use crate::model_registry::context_window_for_model;
use crate::pricing::{PrefixStatus, TurnTelemetry, calculate_turn_cost};
use crate::runtime::AgentRuntime;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub(super) async fn build_turn_telemetry(
        &self,
        route: &TurnRoute,
        usage: Option<&Usage>,
        prefix_hash: u64,
        estimated_context_tokens: u32,
        stream_retries: u32,
    ) -> TurnTelemetry {
        let usage = usage.cloned().unwrap_or_default();
        let turn_cost = calculate_turn_cost(&route.effective_model, &usage);
        let cache_hit = usage.prompt_cache_hit_tokens.unwrap_or(0);
        let cache_miss = usage.prompt_cache_miss_tokens.unwrap_or(0);
        let turn_cache_savings = crate::pricing::cache_savings(&route.effective_model, cache_hit);
        // Single lock acquisition for the whole read-modify-read: a try_lock
        // here would silently drop this turn's cost from the session totals
        // whenever another task holds the state.
        let (
            prior_hash,
            session_cost,
            session_cache_hit_tokens,
            session_cache_miss_tokens,
            session_cache_savings,
            cascade_triggered,
        ) = {
            let mut state = self.state.lock().await;
            let prior_hash = state.last_prefix_hash;
            state.last_prefix_hash = Some(prefix_hash);
            state.session_cost.usd += turn_cost.usd;
            state.session_cost.cny += turn_cost.cny;
            state.session_cache_hit_tokens += u64::from(cache_hit);
            state.session_cache_miss_tokens += u64::from(cache_miss);
            state.session_cache_savings.usd += turn_cache_savings.usd;
            state.session_cache_savings.cny += turn_cache_savings.cny;
            (
                prior_hash,
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
            cache_hit_tokens: usage.prompt_cache_hit_tokens,
            cache_miss_tokens: usage.prompt_cache_miss_tokens,
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
