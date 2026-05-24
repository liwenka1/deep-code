//! DeepSeek-first model registry: ids, aliases, context windows, pricing metadata.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";
pub const DEEPSEEK_V4_FLASH: &str = "deepseek-v4-flash";
pub const AUTO_MODEL: &str = "auto";

/// Approximate context window for DeepSeek V4 family models.
pub const DEEPSEEK_V4_CONTEXT_WINDOW: u32 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPricingMeta {
    /// USD per million input tokens (cache miss).
    pub input_miss_usd: f64,
    /// USD per million input tokens (cache hit).
    pub input_hit_usd: f64,
    /// USD per million output tokens.
    pub output_usd: f64,
    /// CNY per million input tokens (cache miss).
    pub input_miss_cny: f64,
    /// CNY per million input tokens (cache hit).
    pub input_hit_cny: f64,
    /// CNY per million output tokens.
    pub output_cny: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub aliases: Vec<String>,
    pub context_window: u32,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    /// Uses DeepSeek `/beta` chat-completions extensions when true.
    pub beta_endpoint: bool,
    pub pricing: ModelPricingMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResolution {
    pub requested: Option<String>,
    pub resolved_id: String,
    pub used_fallback: bool,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: Vec<ModelInfo>,
    alias_map: HashMap<String, usize>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new(deepseek_default_models())
    }
}

impl ModelRegistry {
    #[must_use]
    pub fn new(models: Vec<ModelInfo>) -> Self {
        let mut alias_map = HashMap::new();
        for (idx, model) in models.iter().enumerate() {
            alias_map
                .entry(normalize_key(&model.id))
                .or_insert(idx);
            for alias in &model.aliases {
                alias_map.entry(normalize_key(alias)).or_insert(idx);
            }
        }
        Self { models, alias_map }
    }

    #[must_use]
    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }

    #[must_use]
    pub fn resolve(&self, requested: Option<&str>) -> ModelResolution {
        let fallback = DEEPSEEK_V4_PRO.to_string();
        let Some(name) = requested.filter(|value| !value.trim().is_empty()) else {
            return ModelResolution {
                requested: None,
                resolved_id: fallback,
                used_fallback: true,
            };
        };

        if normalize_key(name) == normalize_key(AUTO_MODEL) {
            return ModelResolution {
                requested: Some(name.to_string()),
                resolved_id: AUTO_MODEL.to_string(),
                used_fallback: false,
            };
        }

        if let Some(idx) = self.alias_map.get(&normalize_key(name)) {
            return ModelResolution {
                requested: Some(name.to_string()),
                resolved_id: self.models[*idx].id.clone(),
                used_fallback: false,
            };
        }

        ModelResolution {
            requested: Some(name.to_string()),
            resolved_id: name.trim().to_string(),
            used_fallback: true,
        }
    }

    #[must_use]
    pub fn info_for(&self, model_id: &str) -> Option<&ModelInfo> {
        let key = normalize_key(model_id);
        self.alias_map
            .get(&key)
            .map(|idx| &self.models[*idx])
            .or_else(|| {
                self.models
                    .iter()
                    .find(|model| normalize_key(&model.id) == key)
            })
    }
}

#[must_use]
pub fn deepseek_default_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: DEEPSEEK_V4_PRO.to_string(),
            aliases: vec![],
            context_window: DEEPSEEK_V4_CONTEXT_WINDOW,
            supports_tools: true,
            supports_reasoning: true,
            beta_endpoint: true,
            pricing: ModelPricingMeta {
                input_miss_usd: 0.435,
                input_hit_usd: 0.003625,
                output_usd: 0.87,
                input_miss_cny: 3.0,
                input_hit_cny: 0.025,
                output_cny: 6.0,
            },
        },
        ModelInfo {
            id: DEEPSEEK_V4_FLASH.to_string(),
            aliases: vec![
                "deepseek-chat".to_string(),
                "deepseek-reasoner".to_string(),
            ],
            context_window: DEEPSEEK_V4_CONTEXT_WINDOW,
            supports_tools: true,
            supports_reasoning: true,
            beta_endpoint: true,
            pricing: ModelPricingMeta {
                input_miss_usd: 0.14,
                input_hit_usd: 0.0028,
                output_usd: 0.28,
                input_miss_cny: 1.0,
                input_hit_cny: 0.02,
                output_cny: 2.0,
            },
        },
    ]
}

#[must_use]
pub fn context_window_for_model(model: &str) -> u32 {
    ModelRegistry::default()
        .info_for(model)
        .map(|info| info.context_window)
        .unwrap_or(DEEPSEEK_V4_CONTEXT_WINDOW)
}

#[must_use]
pub fn compaction_threshold_for_model(model: &str) -> u32 {
    context_window_for_model(model).saturating_mul(80) / 100
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_pro_and_flash_aliases() {
        let registry = ModelRegistry::default();
        assert_eq!(
            registry.resolve(Some(DEEPSEEK_V4_PRO)).resolved_id,
            DEEPSEEK_V4_PRO
        );
        assert_eq!(
            registry.resolve(Some("deepseek-chat")).resolved_id,
            DEEPSEEK_V4_FLASH
        );
    }

    #[test]
    fn auto_model_is_preserved() {
        let registry = ModelRegistry::default();
        let resolution = registry.resolve(Some("auto"));
        assert_eq!(resolution.resolved_id, AUTO_MODEL);
        assert!(!resolution.used_fallback);
    }

    #[test]
    fn unknown_model_keeps_requested_id() {
        let registry = ModelRegistry::default();
        let resolution = registry.resolve(Some("custom-model"));
        assert_eq!(resolution.resolved_id, "custom-model");
        assert!(resolution.used_fallback);
    }
}
