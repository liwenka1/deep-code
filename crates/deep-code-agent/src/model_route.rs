//! Heuristic auto model selection for DeepSeek pro / flash routing.

use crate::config::AgentConfig;
use crate::i18n::{Lang, TextId, tr, tr_with};
use crate::model_registry::{
    AUTO_MODEL, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelRegistry, ResolutionKind,
};
use crate::reasoning::{ReasoningEffort, ReasoningEffortSetting};
use crate::task_class::{TaskWeight, classify_keyword};

/// Force the strong model once the session fills this fraction of the context
/// window — long contexts need Pro regardless of how the prompt reads.
const CONTEXT_PRESSURE_PERCENT: u64 = 70;

/// What decided a turn's route, for explainable telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    /// A non-negotiable rule (sub-agent, fixed model, context pressure).
    HardRule,
    /// The keyword heuristic (difficulty keyword → Pro), else Flash-first.
    Heuristic,
    /// Cascade escalation: Flash visibly struggled earlier this session, so
    /// later turns run on Pro until the session ends.
    Cascade,
}

impl RouteSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::HardRule => "hard-rule",
            Self::Heuristic => "heuristic",
            Self::Cascade => "cascade",
        }
    }
}

/// Session-state signals that feed routing beyond the prompt text itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct RouteContext {
    /// Estimated tokens already in the session before this turn's request.
    pub context_tokens: u32,
    /// Context window of the model family (0 disables the pressure rule).
    pub context_window: u32,
    /// Cascade escalation latch: Flash already struggled (repeated tool-call
    /// failures) earlier this session, so force Pro for the rest of it.
    pub escalated: bool,
}

impl RouteContext {
    #[must_use]
    fn under_pressure(&self) -> bool {
        self.context_window > 0
            && u64::from(self.context_tokens) * 100
                >= u64::from(self.context_window) * CONTEXT_PRESSURE_PERCENT
    }

    #[must_use]
    fn usage_percent(&self) -> u64 {
        if self.context_window == 0 {
            0
        } else {
            u64::from(self.context_tokens) * 100 / u64::from(self.context_window)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRoute {
    pub requested_model: String,
    pub effective_model: String,
    pub auto_model: bool,
    pub reasoning_setting: ReasoningEffortSetting,
    pub effective_effort: ReasoningEffort,
    pub auto_effort: bool,
    pub used_model_fallback: bool,
    pub route_reason: String,
    pub fallback_reason: Option<String>,
    pub source: RouteSource,
}

impl TurnRoute {
    #[must_use]
    pub fn label(&self) -> String {
        let effort = if self.auto_effort {
            format!("auto→{}", self.effective_effort.short_label())
        } else {
            self.effective_effort.short_label().to_string()
        };
        let mut label = if self.auto_model {
            format!("auto→{} ({})", self.effective_model, effort)
        } else {
            format!("{} ({})", self.effective_model, effort)
        };
        if self.used_model_fallback {
            label.push_str(" (fallback→flash)");
        }
        label
    }
}

/// Cap reasoning effort to what the chosen model accepts: Flash tops out at
/// High (only Pro supports `max`). Applied to whichever model a turn actually
/// runs on, including after an API fallback from Pro to Flash, so we never send
/// `max` to Flash.
#[must_use]
pub fn clamp_effort_to_model(model: &str, effort: ReasoningEffort) -> ReasoningEffort {
    if model == DEEPSEEK_V4_FLASH && effort == ReasoningEffort::Max {
        ReasoningEffort::High
    } else {
        effort
    }
}

/// When auto mode picked Pro and the API call fails, retry once with Flash.
#[must_use]
pub fn api_fallback_model(route: &TurnRoute) -> Option<&'static str> {
    if route.used_model_fallback {
        return None;
    }
    if route.auto_model && route.effective_model == DEEPSEEK_V4_PRO {
        Some(DEEPSEEK_V4_FLASH)
    } else {
        None
    }
}

/// Resolve the concrete model + reasoning effort for one user turn.
#[must_use]
pub fn resolve_turn_route(
    config: &AgentConfig,
    registry: &ModelRegistry,
    user_prompt: &str,
    is_subagent: bool,
    ctx: RouteContext,
    lang: Lang,
) -> TurnRoute {
    let resolution = registry.resolve(Some(config.model.as_str()));
    let auto_model = resolution.resolved_id == AUTO_MODEL;
    let auto_effort = config.reasoning_effort.is_auto();

    if !auto_model {
        let effective_effort = clamp_effort_to_model(
            &resolution.resolved_id,
            config.reasoning_effort.resolve(is_subagent, user_prompt),
        );
        let route_reason = match resolution.kind {
            ResolutionKind::Passthrough => tr_with(
                lang,
                TextId::RouteFixedModelPassthrough,
                &[("model", &resolution.resolved_id)],
            ),
            _ => tr_with(
                lang,
                TextId::RouteFixedModel,
                &[("model", &resolution.resolved_id)],
            ),
        };
        return TurnRoute {
            requested_model: config.model.clone(),
            effective_model: resolution.resolved_id.clone(),
            auto_model: false,
            reasoning_setting: config.reasoning_effort,
            effective_effort,
            auto_effort,
            // Registry-level default/passthrough is not an API fallback: only
            // a live pro→flash retry (streaming) may set this flag.
            used_model_fallback: false,
            route_reason,
            fallback_reason: None,
            source: RouteSource::HardRule,
        };
    }

    let (effective_model, route_reason, source) =
        classify_model(user_prompt, &ctx, config.auto_cost_saving, lang);

    // Effort and model both derive from `task_class`, so they stay coherent.
    let effort = config.reasoning_effort.resolve(is_subagent, user_prompt);
    let effective_effort = clamp_effort_to_model(&effective_model, effort);

    TurnRoute {
        requested_model: config.model.clone(),
        effective_model,
        auto_model: true,
        reasoning_setting: config.reasoning_effort,
        effective_effort,
        auto_effort,
        used_model_fallback: false,
        route_reason,
        fallback_reason: None,
        source,
    }
}

/// Flash-first model selection over the shared [`crate::task_class`] table.
///
/// Returns `(model, human-readable reason, source)`. Pro is forced only by hard
/// facts (cascade escalation, context pressure) or an explicit difficulty
/// keyword. Everything else starts on Flash — cascade escalation (driven by
/// observed tool-call failures) upgrades later turns when Flash actually
/// struggles, so we no longer guess difficulty from prompt length.
pub(crate) fn classify_model(
    input: &str,
    ctx: &RouteContext,
    cost_saving: bool,
    lang: Lang,
) -> (String, String, RouteSource) {
    if ctx.escalated {
        return (
            DEEPSEEK_V4_PRO.to_string(),
            tr(lang, TextId::RouteCascade).to_string(),
            RouteSource::Cascade,
        );
    }

    if ctx.under_pressure() {
        return (
            DEEPSEEK_V4_PRO.to_string(),
            tr_with(
                lang,
                TextId::RouteContextPressure,
                &[
                    ("percent", &ctx.usage_percent().to_string()),
                    ("threshold", &CONTEXT_PRESSURE_PERCENT.to_string()),
                ],
            ),
            RouteSource::HardRule,
        );
    }

    match classify_keyword(input) {
        Some((TaskWeight::Deep, keyword)) => (
            DEEPSEEK_V4_PRO.to_string(),
            tr_with(lang, TextId::RouteKeywordDeep, &[("keyword", keyword)]),
            RouteSource::Heuristic,
        ),
        Some((TaskWeight::Heavy, keyword)) => (
            DEEPSEEK_V4_PRO.to_string(),
            tr_with(lang, TextId::RouteKeywordHeavy, &[("keyword", keyword)]),
            RouteSource::Heuristic,
        ),
        Some((TaskWeight::Borderline, keyword)) if !cost_saving => (
            DEEPSEEK_V4_PRO.to_string(),
            tr_with(
                lang,
                TextId::RouteKeywordBorderline,
                &[("keyword", keyword)],
            ),
            RouteSource::Heuristic,
        ),
        // Everything else (Light keywords, Borderline under cost-saving, no
        // keyword) starts on Flash; cascade upgrades it if Flash struggles.
        _ => (
            DEEPSEEK_V4_FLASH.to_string(),
            tr(lang, TextId::RouteFlashDefault).to_string(),
            RouteSource::Heuristic,
        ),
    }
}

#[cfg(test)]
mod tests;
