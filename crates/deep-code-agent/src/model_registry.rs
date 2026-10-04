//! The catalog of DeepSeek models deep-code can drive, and resolution of
//! user-supplied names (config values, `/model` arguments) to canonical ids.
//!
//! The catalog itself is DATA and lives in `assets/models.toml`, compiled into
//! the binary: which models exist, what they are called, their windows, their
//! capability flags and their prices. This module owns the LOGIC — resolution,
//! lookups, and the shape of an entry. Repricing a model or adding a new one is
//! therefore an edit to that file and to nothing else; its header records where
//! the numbers came from and when they were last checked.
//!
//! The catalog is tiny by design — a handful of first-party entries — so
//! lookups are plain scans over the entry list rather than a prebuilt index.
//! That keeps one source of truth per entry (its id and alias list) and makes
//! "earlier entry wins" conflict handling fall out of iteration order.

use serde::{Deserialize, Serialize};

pub const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";
/// Canonical id for the Flash model. The API renamed it from
/// `deepseek-v4-flash`, which survives as an alias in `assets/models.toml`
/// (still accepted, still billed at the Flash rate), so configs and docs
/// written against the old name keep resolving.
pub const DEEPSEEK_FLASH: &str = "deepseek-flash";
/// Pseudo-model: lets the turn router pick pro/flash per prompt.
pub const AUTO_MODEL: &str = "auto";

/// Context window assumed for an id the catalog does not know — a
/// `Passthrough` model. That can be a model newer than this binary OR an older
/// id pinned from a config, so this is a guess and not a claim about the model.
/// Every catalog entry carries its own window in `assets/models.toml`; this is
/// only the fallback.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 1_000_000;

/// Per-model token pricing in both billing currencies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Availability of a capability the official table does not answer with a plain
/// yes/no.
///
/// FIM completion is why this exists: the table says it works "in non-thinking
/// mode only" (仅非思考模式支持). A `bool` would have to either claim the
/// feature on a thinking-mode request — which the model cannot serve — or deny
/// it where it works. Reach for this type rather than adding a second
/// `supports_x_in_thinking_mode` field if another row grows a qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Unsupported,
    Supported,
    NonThinkingOnly,
}

impl Capability {
    /// Config/report token — the spelling `Deserialize` accepts and `doctor`
    /// prints. On the enum next to its variants like every other setting enum,
    /// so the report matches the real variant instead of lower-casing a Debug
    /// string that a rename would silently change.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Supported => "supported",
            Self::NonThinkingOnly => "non_thinking_only",
        }
    }
}

/// One catalog entry: a canonical model id, the names that map to it, and the
/// facts the official table states about it — capabilities, limits, version.
///
/// Several of these have no consumer yet. They are here because the table
/// states them, they are cheap to carry, and the alternative is re-reading the
/// page when the first consumer lands — vision being the one already scheduled
/// (an image in the prompt has to be refused on Pro, which does not accept one,
/// so `supports_vision` gates the composer rather than the request) and
/// `max_output` being the ceiling any `max_tokens` caller has to respect.
///
/// None of them is write-only: `doctor` reports every field, which is what
/// keeps a wrong value from sitting here unnoticed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    /// Alternative names accepted anywhere a model can be chosen. Includes
    /// legacy ids kept working across releases.
    pub aliases: Vec<String>,
    /// The API's own version string (`DeepSeek-V4-Pro-0813`). Recorded so a
    /// price can be traced to the model build it was quoted for; nothing
    /// branches on it.
    pub version: String,
    /// Emits a thinking (reasoning) trace.
    pub supports_reasoning: bool,
    pub context_window: u32,
    /// Largest response the model will produce — well below `context_window`,
    /// and the ceiling a caller setting `max_tokens` has to respect.
    pub max_output: u32,
    /// Structured JSON output mode.
    pub supports_json_output: bool,
    /// Tool/function calling.
    pub supports_tools: bool,
    /// Serves the OpenAI Responses shape.
    pub supports_responses_api: bool,
    /// Serves the Anthropic Messages shape.
    pub supports_anthropic_api: bool,
    /// Beta chat-prefix completion: continuing an assistant turn from a
    /// supplied prefix.
    pub supports_prefix_completion: bool,
    /// Beta fill-in-the-middle completion, non-thinking mode only.
    pub fim_completion: Capability,
    /// Accepts images in the prompt. False on Pro — the flagship is the one
    /// model here that cannot take one.
    pub supports_vision: bool,
    /// Concurrent-request cap the table lists per model. An account-level
    /// figure, not a per-process one, so it is a hint for backoff rather than
    /// a budget to schedule against.
    pub concurrency_limit: u32,
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
    /// * `AUTO_MODEL` → passed through untouched so per-turn routing stays
    ///   in charge.
    /// * A catalog id or alias (case/whitespace-insensitive) → its canonical id.
    /// * Anything else → trusted as-is (it may be a model newer than this
    ///   binary), but flagged so callers can warn.
    #[must_use]
    pub fn resolve(&self, requested: Option<&str>) -> ModelResolution {
        let Some(asked) = requested.filter(|value| !value.trim().is_empty()) else {
            return ModelResolution {
                resolved_id: DEEPSEEK_V4_PRO.to_string(),
                kind: ResolutionKind::DefaultApplied,
            };
        };

        let answer =
            |resolved_id: String, kind: ResolutionKind| ModelResolution { resolved_id, kind };

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

/// TOML shape of `assets/models.toml`: one array-of-tables entry per model.
///
/// `deny_unknown_fields` on this and on every struct below is what makes a
/// misspelled or retired key an error instead of a no-op. Serde ignores unknown
/// keys by default, so `supports_video = true` would otherwise sit in the file
/// looking authoritative while nothing read it, and a key renamed out from
/// under a consumer would fail silently in the direction of "the feature is
/// off" — the one direction nobody notices.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
    model: Vec<ModelInfo>,
}

/// The built-in catalog, parsed from the TOML data file compiled into the
/// binary.
///
/// Every field of [`ModelInfo`] — id, aliases, context window, capability flags
/// and the six prices — comes from that file. Repricing a model or adding a new
/// one is a data edit that never reaches this module's logic.
///
/// `expect` rather than a fallback: the file ships with the binary, so a parse
/// failure is a broken build, not a runtime condition — the same reasoning as
/// the generated config template in `config::write`. The test
/// `embedded_catalog_is_well_formed` is what keeps a broken file out of a
/// release; this call is the last line of defence, not the first.
#[must_use]
pub fn deepseek_default_models() -> Vec<ModelInfo> {
    toml::from_str::<ModelsFile>(include_str!("../assets/models.toml"))
        .expect("embedded assets/models.toml parses")
        .model
}

/// The built-in catalog, built once.
///
/// The free functions below — and `pricing`'s — are called per turn, per
/// request and (through `estimate_token_count`'s callers) per rendered frame,
/// and each one used to construct a fresh `ModelRegistry`: two `ModelInfo`
/// values with their `Vec<String>` alias lists, allocated and dropped to
/// answer a question about a static table. Nothing about the built-in catalog
/// can change under a running process, so it is built once and borrowed.
///
/// Parsing the catalog from a TOML asset rather than spelling it out in Rust
/// makes that argument stronger, not weaker: the parse costs real time, and it
/// happens exactly once here instead of once per lookup.
///
/// `ModelRegistry::default()` stays as it was: callers that want an owned
/// catalog (`runtime`, `doctor`, `/model`) keep getting one.
pub(crate) fn builtin_registry() -> &'static ModelRegistry {
    static REGISTRY: std::sync::LazyLock<ModelRegistry> =
        std::sync::LazyLock::new(ModelRegistry::default);
    &REGISTRY
}

/// Context window for `model`: the catalog entry's own window, or
/// [`DEFAULT_CONTEXT_WINDOW`] — a guess, not a claim about the model — when the
/// id is not in the catalog.
#[must_use]
pub fn context_window_for_model(model: &str) -> u32 {
    builtin_registry()
        .info_for(model)
        .map_or(DEFAULT_CONTEXT_WINDOW, |entry| entry.context_window)
}

/// Whether `model` accepts images in the prompt.
///
/// `None` means the catalog has no row for this id — a self-hosted proxy name, a
/// name from a newer table, or a hand-written `provider.model`. Unknown is
/// deliberately not `false`: refusing to send an image to a model we merely have
/// no row for would break the setups that cannot be tested here, and the
/// server's verdict is authoritative anyway. `Some(false)` is a claim we can act
/// on, because it comes from the table.
#[must_use]
pub fn supports_vision_for_model(model: &str) -> Option<bool> {
    builtin_registry()
        .info_for(model)
        .map(|entry| entry.supports_vision)
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
        for id in [DEEPSEEK_V4_PRO, DEEPSEEK_FLASH] {
            let resolution = registry.resolve(Some(id));
            assert_eq!(resolution.resolved_id, id);
            assert_eq!(resolution.kind, ResolutionKind::Resolved);
        }
    }

    /// Every name the API answers to for Flash resolves to the one canonical
    /// id, ignoring case and surrounding whitespace. `deepseek-v4-flash` is the
    /// one that matters most: it is what releases up to 0.4.10 persisted into
    /// `provider.model`, and a missing alias would send it through as a
    /// `Passthrough` id the API 400s.
    #[test]
    fn every_flash_name_resolves_to_the_canonical_id() {
        let registry = ModelRegistry::default();
        for name in [
            "deepseek-flash",
            "DeepSeek-Flash",
            "  deepseek-flash ",
            "deepseek-v4-flash",
            "flash",
        ] {
            let resolution = registry.resolve(Some(name));
            assert_eq!(resolution.resolved_id, DEEPSEEK_FLASH, "for {name:?}");
            assert_eq!(resolution.kind, ResolutionKind::Resolved, "for {name:?}");
        }
    }

    /// The catalog is a data file now, so no compiler check stands between a bad
    /// edit and a shipped binary. This is that check: every entry parses, names
    /// are non-empty and claimed once, limits are coherent, prices are prices,
    /// and the two ids the crate names by constant are actually present.
    ///
    /// The alias check earns its place: `entry_matching` returns the FIRST match,
    /// so an alias claimed twice silently hands one entry's lookups — including
    /// its pricing — to another, with nothing else failing.
    ///
    /// What this test cannot see is a key that parses but means nothing: no
    /// struct here can observe an unknown one. `#[serde(deny_unknown_fields)]`
    /// on the file's structs is what turns that into a parse failure, so a
    /// retired key or a typo cannot sit in the file looking authoritative.
    #[test]
    fn embedded_catalog_is_well_formed() {
        let registry = ModelRegistry::default();
        for id in [DEEPSEEK_V4_PRO, DEEPSEEK_FLASH] {
            assert!(
                registry.info_for(id).is_some(),
                "{id} is named by a constant but missing from assets/models.toml"
            );
        }

        let mut claimed: Vec<String> = Vec::new();
        for entry in registry.list() {
            assert!(!entry.id.trim().is_empty(), "a catalog entry has no id");
            assert!(entry.context_window > 0, "{} has no window", entry.id);
            assert!(
                entry.max_output > 0 && entry.max_output < entry.context_window,
                "{}: max_output={} must be positive and below the {}-token window; a caller \
                 sizing `max_tokens` against it would otherwise ask for more than the model can \
                 hold in one response",
                entry.id,
                entry.max_output,
                entry.context_window
            );
            for name in
                std::iter::once(entry.id.as_str()).chain(entry.aliases.iter().map(String::as_str))
            {
                assert!(!name.trim().is_empty(), "{} has a blank name", entry.id);
                // Fold exactly what `names_equal` folds. A plain comparison would
                // miss "Flash" colliding with "flash", and since `entry_matching`
                // takes the FIRST match, the collision would quietly hand one
                // entry's lookups — pricing included — to another. That is the
                // failure this loop exists to catch, so getting the comparison
                // wrong here would leave it uncaught.
                let folded = name.trim().to_ascii_lowercase();
                assert!(
                    !claimed.contains(&folded),
                    "{name:?} is claimed twice in assets/models.toml (names compare ignoring \
                     case and surrounding whitespace); the earlier entry wins, so the later one \
                     is unreachable"
                );
                // `resolve` answers the `auto` sentinel before it consults the
                // catalog, so an entry claiming that name can never be reached.
                assert!(
                    folded != AUTO_MODEL,
                    "{} claims the {AUTO_MODEL:?} sentinel, which `resolve` never looks up",
                    entry.id
                );
                claimed.push(folded);
            }

            let pricing = &entry.pricing;
            for (field, value) in [
                ("input_miss_usd", pricing.input_miss_usd),
                ("input_hit_usd", pricing.input_hit_usd),
                ("output_usd", pricing.output_usd),
                ("input_miss_cny", pricing.input_miss_cny),
                ("input_hit_cny", pricing.input_hit_cny),
                ("output_cny", pricing.output_cny),
            ] {
                assert!(
                    value.is_finite() && value >= 0.0,
                    "{} {field} is {value}, which is not a price",
                    entry.id
                );
            }
            // A cache hit billed above a cache miss is always a typo, and it
            // turns `cache_savings` negative — the report would then claim the
            // cache cost the user money.
            assert!(
                pricing.input_hit_usd <= pricing.input_miss_usd
                    && pricing.input_hit_cny <= pricing.input_miss_cny,
                "{} bills a cache hit above a cache miss",
                entry.id
            );
        }
    }

    /// `as_setting` and the serde spelling are two renderings of one state, and
    /// only the second is exercised in anger — the data file is parsed with it.
    /// This holds one direction: every token `as_setting` emits must be one
    /// serde accepts, which is what breaks if `rename_all` is edited.
    ///
    /// The other direction is deliberately not covered. Catching a spelling
    /// serde accepts but nothing prints (a hand-added `#[serde(alias = ...)]`)
    /// would mean asserting the ABSENCE of spellings, and serde offers no way
    /// to enumerate them. Such an alias is a deliberate act; nothing here can
    /// catch one added by accident.
    ///
    /// `pricing`'s `CostCurrency` round-trip test is the same guard for the same
    /// reason.
    #[test]
    fn capability_setting_token_is_the_spelling_serde_accepts() {
        #[derive(Deserialize)]
        struct Probe {
            value: Capability,
        }
        for capability in [
            Capability::Unsupported,
            Capability::Supported,
            Capability::NonThinkingOnly,
        ] {
            let probe: Probe = toml::from_str(&format!("value = \"{}\"", capability.as_setting()))
                .unwrap_or_else(|error| {
                    panic!(
                        "{capability:?} emits {:?}, which the data file cannot parse: {error}",
                        capability.as_setting()
                    )
                });
            assert_eq!(probe.value, capability);
        }
    }

    #[test]
    fn auto_passes_through_for_the_router() {
        let resolution = ModelRegistry::default().resolve(Some("Auto"));
        assert_eq!(resolution.resolved_id, AUTO_MODEL);
        assert_eq!(resolution.kind, ResolutionKind::Resolved);
    }

    /// The short names the README, config docs and the CI bot advertise
    /// (`provider.model = pro|flash`) must resolve to real ids, not pass through
    /// as unknown ones the API 400s on. This is the one place they were NOT
    /// mapped — the TUI's `/model` did it by hand, config/env/bot did not.
    #[test]
    fn pro_and_flash_short_names_resolve() {
        let registry = ModelRegistry::default();
        for (name, expected) in [
            ("pro", DEEPSEEK_V4_PRO),
            ("PRO", DEEPSEEK_V4_PRO),
            ("flash", DEEPSEEK_FLASH),
            ("  Flash ", DEEPSEEK_FLASH),
        ] {
            let resolution = registry.resolve(Some(name));
            assert_eq!(resolution.resolved_id, expected, "for {name:?}");
            assert_eq!(
                resolution.kind,
                ResolutionKind::Resolved,
                "{name:?} must not pass through as an unknown id"
            );
        }
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
    }

    #[test]
    fn info_lookup_works_through_aliases() {
        let registry = ModelRegistry::default();
        let entry = registry.info_for("deepseek-v4-flash").expect("known alias");
        assert_eq!(entry.id, DEEPSEEK_FLASH);
        assert!(registry.info_for("no-such-model").is_none());
    }

    #[test]
    fn compaction_threshold_is_80_percent_of_window() {
        // The expected window comes from the CATALOG, not from
        // `DEFAULT_CONTEXT_WINDOW`. Naming the fallback constant here would also
        // pin "Pro's window equals the fallback", which is a coincidence today
        // and a false constraint the moment a model with a different window is
        // added. The 80% arithmetic is what this test is for.
        let window = builtin_registry()
            .info_for(DEEPSEEK_V4_PRO)
            .expect("the catalog names Pro")
            .context_window;
        assert_eq!(
            compaction_threshold_for_model(DEEPSEEK_V4_PRO),
            window / 100 * 80
        );
        // Ids the catalog does not know fall back to the default window.
        assert_eq!(context_window_for_model("mystery"), DEFAULT_CONTEXT_WINDOW);
    }
}
