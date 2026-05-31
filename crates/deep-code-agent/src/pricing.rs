//! DeepSeek cost estimation with USD and CNY display.

use serde::{Deserialize, Serialize};

use crate::model::Usage;
use crate::model_registry::{ModelPricingMeta, ModelRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CostCurrency {
    #[default]
    Cny,
    Usd,
}

impl CostCurrency {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "usd" | "dollar" | "dollars" | "$" => Some(Self::Usd),
            "cny" | "rmb" | "yuan" | "¥" => Some(Self::Cny),
            _ => None,
        }
    }

    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Usd => "$",
            Self::Cny => "¥",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub usd: f64,
    pub cny: f64,
}

impl CostEstimate {
    #[must_use]
    pub fn is_positive(self) -> bool {
        self.usd > 0.0 || self.cny > 0.0
    }

    #[must_use]
    pub fn amount(self, currency: CostCurrency) -> f64 {
        match currency {
            CostCurrency::Usd => self.usd,
            CostCurrency::Cny => self.cny,
        }
    }

    #[must_use]
    pub fn format(self, currency: CostCurrency) -> String {
        let amount = self.amount(currency);
        if amount <= 0.0 {
            return format!("{}0.00", currency.symbol());
        }
        format!("{}{:.4}", currency.symbol(), amount)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefixStatus {
    FirstTurn,
    Stable,
    Changed,
}

impl PrefixStatus {
    #[must_use]
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::FirstTurn => "prefix 首回合",
            Self::Stable => "prefix 稳定",
            Self::Changed => "prefix 变动",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnTelemetry {
    pub route_label: String,
    pub effective_model: String,
    pub reasoning_effort: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_hit_tokens: Option<u32>,
    pub cache_miss_tokens: Option<u32>,
    pub prefix_status: PrefixStatus,
    #[serde(default)]
    pub route_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    pub context_window: u32,
    pub estimated_context_tokens: u32,
    pub context_usage_percent: u8,
    pub near_compaction_threshold: bool,
    pub used_model_fallback: bool,
    pub turn_cost: CostEstimate,
    pub session_cost: CostEstimate,
}

#[must_use]
pub fn calculate_turn_cost(model: &str, usage: &Usage) -> CostEstimate {
    let Some(pricing) = ModelRegistry::default()
        .info_for(model)
        .map(|info| info.pricing.clone())
    else {
        return CostEstimate::default();
    };
    calculate_with_pricing(&pricing, usage)
}

fn calculate_with_pricing(pricing: &ModelPricingMeta, usage: &Usage) -> CostEstimate {
    let input = usage.input_tokens();
    let output = usage.output_tokens();
    let hit = usage.prompt_cache_hit_tokens.unwrap_or(0);
    let miss = usage
        .prompt_cache_miss_tokens
        .unwrap_or_else(|| input.saturating_sub(hit));
    let accounted = hit.saturating_add(miss);
    let uncategorized = input.saturating_sub(accounted);
    let miss_total = miss.saturating_add(uncategorized);
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    let effective_output = output.saturating_add(reasoning);

    CostEstimate {
        usd: tier_cost(hit, pricing.input_hit_usd)
            + tier_cost(miss_total, pricing.input_miss_usd)
            + tier_cost(effective_output, pricing.output_usd),
        cny: tier_cost(hit, pricing.input_hit_cny)
            + tier_cost(miss_total, pricing.input_miss_cny)
            + tier_cost(effective_output, pricing.output_cny),
    }
}

fn tier_cost(tokens: u32, per_million: f64) -> f64 {
    (tokens as f64 / 1_000_000.0) * per_million
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_registry::DEEPSEEK_V4_FLASH;

    #[test]
    fn cache_hit_lowers_cost() {
        let usage = Usage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(100),
            prompt_cache_hit_tokens: Some(900),
            prompt_cache_miss_tokens: Some(100),
            ..Usage::default()
        };
        let full_miss = Usage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(100),
            ..Usage::default()
        };
        let hit_cost = calculate_turn_cost(DEEPSEEK_V4_FLASH, &usage);
        let miss_cost = calculate_turn_cost(DEEPSEEK_V4_FLASH, &full_miss);
        assert!(hit_cost.cny < miss_cost.cny);
    }
}
