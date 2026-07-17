//! The catalog of DeepSeek models deep-code can drive, and resolution of
//! user-supplied names (config values, `/model` arguments) to canonical ids.
//!
//! The catalog is tiny by design — a handful of first-party entries — so
//! lookups are plain scans over the entry list rather than a prebuilt index.
//! That keeps one source of truth per entry (its id and alias list) and makes
//! "earlier entry wins" conflict handling fall out of iteration order.

use serde::{Deserialize, Serialize};

pub const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";
pub const DEEPSEEK_V4_FLASH: &str = "deepseek-v4-flash";
/// Pseudo-model: lets the turn router pick pro/flash per prompt.
pub const AUTO_MODEL: &str = "auto";

/// Context window shared by the DeepSeek V4 family.
pub const DEEPSEEK_V4_CONTEXT_WINDOW: u32 = 1_000_000;

/// Per-model token pricing in both billing currencies.
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

/// One catalog entry: a canonical model id plus the names that map to it and
/// the capabilities the runtime needs to know about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// Alternative names accepted anywhere a model can be chosen. Includes
    /// legacy ids kept working across releases.
    pub aliases: Vec<String>,
    pub context_window: u32,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    /// Uses DeepSeek `/beta` chat-completions extensions when true.
    pub beta_endpoint: bool,
    pub pricing: ModelPricingMeta,
}

/// How a requested model name mapped to a concrete id. `DefaultApplied` and
/// `Passthrough` are deliberately distinct: neither is an API-level fallback,
/// and only `Passthrough` warrants a "unknown model" warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionKind {
    /// Catalog id, alias, or `auto` — the catalog recognized the name.
    Resolved,
    /// Nothing usable requested; the flagship default was applied.
    DefaultApplied,
    /// Unrecognized name trusted as-is (may be newer than this binary).
    Passthrough,
}

/// Outcome of turning a requested model name into a concrete id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResolution {
    /// What the caller asked for, verbatim (`None` when nothing usable was given).
    pub requested: Option<String>,
    /// The id the runtime should actually use.
    pub resolved_id: String,
    /// How the catalog arrived at `resolved_id`.
    pub kind: ResolutionKind,
}

/// The model catalog. Construct via [`Default`] for the built-in DeepSeek
/// entries, or [`ModelRegistry::new`] to supply a custom catalog.
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    catalog: Vec<ModelInfo>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new(deepseek_default_models())
    }
}

impl ModelRegistry {
    #[must_use]
    pub fn new(catalog: Vec<ModelInfo>) -> Self {
        Self { catalog }
    }

    /// All catalog entries, in priority order.
    #[must_use]
    pub fn list(&self) -> &[ModelInfo] {
        &self.catalog
    }

    /// Turn a requested model name into a concrete id.
    ///
    /// * Nothing (or only whitespace) requested → the flagship default,
    ///   flagged as a fallback.
    /// * [`AUTO_MODEL`] → passed through untouched so per-turn routing stays
    ///   in charge.
    /// * A catalog id or alias (case/whitespace-insensitive) → its canonical id.
    /// * Anything else → trusted as-is (it may be a model newer than this
    ///   binary), but flagged so callers can warn.
    #[must_use]
    pub fn resolve(&self, requested: Option<&str>) -> ModelResolution {
        let Some(asked) = requested.filter(|value| !value.trim().is_empty()) else {
            return ModelResolution {
                requested: None,
                resolved_id: DEEPSEEK_V4_PRO.to_string(),
                kind: ResolutionKind::DefaultApplied,
            };
        };

        let answer = |resolved_id: String, kind: ResolutionKind| ModelResolution {
            requested: Some(asked.to_string()),
            resolved_id,
            kind,
        };

        if names_equal(asked, AUTO_MODEL) {
            return answer(AUTO_MODEL.to_string(), ResolutionKind::Resolved);
        }
        if let Some(entry) = self.entry_matching(asked) {
            return answer(entry.id.clone(), ResolutionKind::Resolved);
        }
        answer(asked.trim().to_string(), ResolutionKind::Passthrough)
    }

    /// Catalog metadata for a model id or alias, if it is a known entry.
    #[must_use]
    pub fn info_for(&self, model_id: &str) -> Option<&ModelInfo> {
        self.entry_matching(model_id)
    }

    /// Scan for the entry whose id or alias list covers `name`. Earlier
    /// entries win if two entries ever claim the same name.
    fn entry_matching(&self, name: &str) -> Option<&ModelInfo> {
        self.catalog.iter().find(|entry| {
            names_equal(&entry.id, name)
                || entry.aliases.iter().any(|alias| names_equal(alias, name))
        })
    }
}

/// Model names compare ignoring surrounding whitespace and ASCII case.
fn names_equal(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The built-in catalog: DeepSeek V4 Pro (flagship) and V4 Flash (fast/cheap).
/// Flash keeps the legacy `deepseek-chat` / `deepseek-reasoner` ids working.
#[must_use]
pub fn deepseek_default_models() -> Vec<ModelInfo> {
    let v4_entry = |id: &str, aliases: &[&str], pricing: ModelPricingMeta| ModelInfo {
        id: id.to_string(),
        aliases: aliases.iter().map(ToString::to_string).collect(),
        context_window: DEEPSEEK_V4_CONTEXT_WINDOW,
        supports_tools: true,
        supports_reasoning: true,
        beta_endpoint: true,
        pricing,
    };
    vec![
        v4_entry(
            DEEPSEEK_V4_PRO,
            &[],
            ModelPricingMeta {
                input_miss_usd: 0.435,
                input_hit_usd: 0.003625,
                output_usd: 0.87,
                input_miss_cny: 3.0,
                input_hit_cny: 0.025,
                output_cny: 6.0,
            },
        ),
        v4_entry(
            DEEPSEEK_V4_FLASH,
            &["deepseek-chat", "deepseek-reasoner"],
            ModelPricingMeta {
                input_miss_usd: 0.14,
                input_hit_usd: 0.0028,
                output_usd: 0.28,
                input_miss_cny: 1.0,
                input_hit_cny: 0.02,
                output_cny: 2.0,
            },
        ),
    ]
}

/// Context window for `model`, falling back to the V4 family window for
/// unknown ids (all currently supported models share it).
#[must_use]
pub fn context_window_for_model(model: &str) -> u32 {
    ModelRegistry::default()
        .info_for(model)
        .map_or(DEEPSEEK_V4_CONTEXT_WINDOW, |entry| entry.context_window)
}

/// Token count at which history compaction should kick in: 80% of the model's
/// window, leaving headroom for the reply and compaction overhead.
#[must_use]
pub fn compaction_threshold_for_model(model: &str) -> u32 {
    context_window_for_model(model).saturating_mul(80) / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_ids_resolve_to_themselves() {
        let registry = ModelRegistry::default();
        for id in [DEEPSEEK_V4_PRO, DEEPSEEK_V4_FLASH] {
            let resolution = registry.resolve(Some(id));
            assert_eq!(resolution.resolved_id, id);
            assert_eq!(resolution.kind, ResolutionKind::Resolved);
            assert_eq!(resolution.requested.as_deref(), Some(id));
        }
    }

    #[test]
    fn legacy_names_map_to_flash_ignoring_case() {
        let registry = ModelRegistry::default();
        for legacy in ["deepseek-chat", "DeepSeek-Reasoner", "  deepseek-chat "] {
            let resolution = registry.resolve(Some(legacy));
            assert_eq!(resolution.resolved_id, DEEPSEEK_V4_FLASH, "for {legacy:?}");
            assert_eq!(resolution.kind, ResolutionKind::Resolved);
        }
    }

    #[test]
    fn auto_passes_through_for_the_router() {
        let resolution = ModelRegistry::default().resolve(Some("Auto"));
        assert_eq!(resolution.resolved_id, AUTO_MODEL);
        assert_eq!(resolution.kind, ResolutionKind::Resolved);
    }

    #[test]
    fn missing_or_blank_request_defaults_to_pro() {
        let registry = ModelRegistry::default();
        for request in [None, Some(""), Some("   ")] {
            let resolution = registry.resolve(request);
            assert_eq!(resolution.resolved_id, DEEPSEEK_V4_PRO);
            assert_eq!(resolution.kind, ResolutionKind::DefaultApplied);
        }
    }

    #[test]
    fn unlisted_id_is_trusted_but_flagged() {
        let resolution = ModelRegistry::default().resolve(Some(" experimental-v5 "));
        assert_eq!(resolution.resolved_id, "experimental-v5");
        assert_eq!(resolution.kind, ResolutionKind::Passthrough);
        assert_eq!(resolution.requested.as_deref(), Some(" experimental-v5 "));
    }

    #[test]
    fn info_lookup_works_through_aliases() {
        let registry = ModelRegistry::default();
        let entry = registry.info_for("deepseek-reasoner").expect("known alias");
        assert_eq!(entry.id, DEEPSEEK_V4_FLASH);
        assert!(registry.info_for("no-such-model").is_none());
    }

    #[test]
    fn compaction_threshold_is_80_percent_of_window() {
        assert_eq!(
            compaction_threshold_for_model(DEEPSEEK_V4_PRO),
            DEEPSEEK_V4_CONTEXT_WINDOW / 100 * 80
        );
        // Unknown models inherit the family window.
        assert_eq!(
            context_window_for_model("mystery"),
            DEEPSEEK_V4_CONTEXT_WINDOW
        );
    }
}
