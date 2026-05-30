use crate::auto_mode::TurnRoute;
use crate::client::LlmClient;
use crate::compaction::{context_usage_percent, effective_compaction_threshold};
use crate::model::Usage;
use crate::model_registry::context_window_for_model;
use crate::pricing::{PrefixStatus, TurnTelemetry, calculate_turn_cost};
use crate::runtime::AgentRuntime;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub(super) fn build_turn_telemetry(
        &self,
        route: &TurnRoute,
        usage: Option<&Usage>,
        prefix_hash: u64,
        estimated_context_tokens: u32,
    ) -> TurnTelemetry {
        let usage = usage.cloned().unwrap_or_default();
        let turn_cost = calculate_turn_cost(&route.effective_model, &usage);
        let prior_hash = self
            .state
            .try_lock()
            .ok()
            .and_then(|state| state.last_prefix_hash);
        let prefix_status = match prior_hash {
            None => PrefixStatus::FirstTurn,
            Some(previous) if previous == prefix_hash => PrefixStatus::Stable,
            Some(_) => PrefixStatus::Changed,
        };
        if let Ok(mut state) = self.state.try_lock() {
            state.last_prefix_hash = Some(prefix_hash);
            state.session_cost.usd += turn_cost.usd;
            state.session_cost.cny += turn_cost.cny;
        }
        let session_cost = self
            .state
            .try_lock()
            .map(|state| state.session_cost)
            .unwrap_or(turn_cost);
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
            prefix_status,
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
            turn_cost,
            session_cost,
        }
    }
}
