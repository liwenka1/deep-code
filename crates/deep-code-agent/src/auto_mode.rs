//! Heuristic auto model selection for DeepSeek pro / flash routing.

use crate::config::AgentConfig;
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelRegistry};
use crate::reasoning::{ReasoningEffort, ReasoningEffortSetting};
use crate::task_class::{TaskWeight, classify_keyword};

/// Force the strong model once the session fills this fraction of the context
/// window — long contexts need Pro regardless of how the prompt reads.
const CONTEXT_PRESSURE_PERCENT: u64 = 70;
/// Prompts shorter than this (and free of difficulty keywords) default to Flash.
const SHORT_PROMPT_CHARS: usize = 100;

/// What decided a turn's route, for explainable telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    /// A non-negotiable rule (sub-agent, fixed model, context pressure).
    HardRule,
    /// The keyword/length heuristic.
    Heuristic,
    /// The Flash classifier resolved an otherwise-ambiguous turn.
    FlashRouter,
}

impl RouteSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::HardRule => "hard-rule",
            Self::Heuristic => "heuristic",
            Self::FlashRouter => "flash-router",
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

/// A model-selection outcome from the deterministic heuristic.
///
/// `Ambiguous` is the gray zone the Phase-2 Flash router resolves; callers
/// without a router fall back to Flash.
pub(crate) enum ModelClass {
    Decisive {
        model: String,
        reason: String,
        source: RouteSource,
    },
    Ambiguous {
        reason: String,
    },
}

/// Resolve the concrete model + reasoning effort for one user turn.
#[must_use]
pub fn resolve_turn_route(
    config: &AgentConfig,
    registry: &ModelRegistry,
    user_prompt: &str,
    is_subagent: bool,
    ctx: RouteContext,
) -> TurnRoute {
    let resolution = registry.resolve(Some(config.model.as_str()));
    let auto_model = resolution.resolved_id == AUTO_MODEL;
    let auto_effort = config.reasoning_effort.is_auto();

    if !auto_model {
        let effective_effort = clamp_effort_to_model(
            &resolution.resolved_id,
            config.reasoning_effort.resolve(is_subagent, user_prompt),
        );
        return TurnRoute {
            requested_model: config.model.clone(),
            effective_model: resolution.resolved_id.clone(),
            auto_model: false,
            reasoning_setting: config.reasoning_effort,
            effective_effort,
            auto_effort,
            used_model_fallback: resolution.used_fallback,
            route_reason: format!("固定模型配置：{}", resolution.resolved_id),
            fallback_reason: None,
            source: RouteSource::Heuristic,
        };
    }

    let (effective_model, route_reason, source) =
        match classify_model(user_prompt, &ctx, config.auto_cost_saving) {
            ModelClass::Decisive {
                model,
                reason,
                source,
            } => (model, reason, source),
            ModelClass::Ambiguous { reason } => {
                // No router yet: the gray zone defaults to Flash.
                (
                    DEEPSEEK_V4_FLASH.to_string(),
                    reason,
                    RouteSource::Heuristic,
                )
            }
        };

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

/// Short prompts → Flash; difficulty keywords or long prompts → Pro.
#[must_use]
pub fn select_auto_model(input: &str, cost_saving: bool) -> String {
    select_auto_model_with_reason(input, cost_saving).0
}

/// Model + human-readable reason for status surfaces (no session context).
#[must_use]
pub fn select_auto_model_with_reason(input: &str, cost_saving: bool) -> (String, String) {
    match classify_model(input, &RouteContext::default(), cost_saving) {
        ModelClass::Decisive { model, reason, .. } => (model, reason),
        ModelClass::Ambiguous { reason } => (DEEPSEEK_V4_FLASH.to_string(), reason),
    }
}

/// Deterministic model selection over the shared [`crate::task_class`] table.
/// Priority: context pressure → difficulty keyword → length, with the
/// 100‑to‑threshold gray zone left `Ambiguous` for the Flash router.
pub(crate) fn classify_model(input: &str, ctx: &RouteContext, cost_saving: bool) -> ModelClass {
    if ctx.under_pressure() {
        return ModelClass::Decisive {
            model: DEEPSEEK_V4_PRO.to_string(),
            reason: format!(
                "上下文占用约 {}%（≥{CONTEXT_PRESSURE_PERCENT}% 阈值），使用 Pro 处理长上下文",
                ctx.usage_percent()
            ),
            source: RouteSource::HardRule,
        };
    }

    match classify_keyword(input) {
        Some((TaskWeight::Deep, keyword)) => ModelClass::Decisive {
            model: DEEPSEEK_V4_PRO.to_string(),
            reason: format!("命中调试/报错类关键词“{keyword}”，使用 Pro 配深推理"),
            source: RouteSource::Heuristic,
        },
        Some((TaskWeight::Heavy, keyword)) => ModelClass::Decisive {
            model: DEEPSEEK_V4_PRO.to_string(),
            reason: format!("命中复杂任务关键词“{keyword}”，使用 Pro 以获得更强推理和工具规划能力"),
            source: RouteSource::Heuristic,
        },
        Some((TaskWeight::Borderline, keyword)) if !cost_saving => ModelClass::Decisive {
            model: DEEPSEEK_V4_PRO.to_string(),
            reason: format!("任务包含“{keyword}”，且未开启成本优先，使用 Pro"),
            source: RouteSource::Heuristic,
        },
        // Borderline under cost-saving and Light keywords fall through to the
        // length check below (Light shouldn't force Flash on a long prompt).
        _ => classify_by_length(input, cost_saving),
    }
}

fn classify_by_length(input: &str, cost_saving: bool) -> ModelClass {
    let len = input.chars().count();
    if len < SHORT_PROMPT_CHARS {
        return ModelClass::Decisive {
            model: DEEPSEEK_V4_FLASH.to_string(),
            reason: "短提示优先使用 Flash，降低延迟和成本".to_string(),
            source: RouteSource::Heuristic,
        };
    }
    let long_threshold = if cost_saving { 1_000 } else { 500 };
    if len > long_threshold {
        return ModelClass::Decisive {
            model: DEEPSEEK_V4_PRO.to_string(),
            reason: format!("输入长度 {len} 超过阈值 {long_threshold}，使用 Pro 处理长上下文"),
            source: RouteSource::Heuristic,
        };
    }
    ModelClass::Ambiguous {
        reason: format!("中等长度（{len} 字）且无明确难度信号，待进一步判定"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::CostCurrency;

    #[test]
    fn short_prompt_routes_to_flash() {
        assert_eq!(select_auto_model("hello", false), DEEPSEEK_V4_FLASH);
    }

    #[test]
    fn debug_routes_to_pro() {
        assert_eq!(
            select_auto_model("please debug this error", false),
            DEEPSEEK_V4_PRO
        );
    }

    #[test]
    fn chinese_refactor_routes_to_pro() {
        assert_eq!(
            select_auto_model(
                "\u{5e2e}\u{6211}\u{91cd}\u{6784}\u{8fd9}\u{4e2a}\u{6a21}\u{5757}",
                false
            ),
            DEEPSEEK_V4_PRO
        );
    }

    #[test]
    fn resolve_turn_route_auto_model_and_effort() {
        let config = AgentConfig {
            model: AUTO_MODEL.to_string(),
            reasoning_effort: ReasoningEffortSetting::Auto,
            ..AgentConfig::default()
        };
        let route = resolve_turn_route(
            &config,
            &ModelRegistry::default(),
            "debug crash",
            false,
            RouteContext::default(),
        );
        assert!(route.auto_model);
        assert!(route.auto_effort);
        assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
        assert_eq!(route.effective_effort, ReasoningEffort::Max);
        assert!(route.route_reason.contains("debug"));
    }

    #[test]
    fn resolve_subagent_uses_low_effort_in_auto_mode() {
        let config = AgentConfig {
            reasoning_effort: ReasoningEffortSetting::Auto,
            ..AgentConfig::default()
        };
        let route = resolve_turn_route(
            &config,
            &ModelRegistry::default(),
            "debug crash",
            true,
            RouteContext::default(),
        );
        assert_eq!(route.effective_effort, ReasoningEffort::Low);
    }

    #[test]
    fn flash_never_requests_max_effort() {
        assert_eq!(
            clamp_effort_to_model(DEEPSEEK_V4_FLASH, ReasoningEffort::Max),
            ReasoningEffort::High
        );
        // High and below pass through; Pro keeps Max.
        assert_eq!(
            clamp_effort_to_model(DEEPSEEK_V4_FLASH, ReasoningEffort::High),
            ReasoningEffort::High
        );
        assert_eq!(
            clamp_effort_to_model(DEEPSEEK_V4_PRO, ReasoningEffort::Max),
            ReasoningEffort::Max
        );
    }

    #[test]
    fn fixed_flash_clamps_explicit_max_effort_to_high() {
        // A fixed Flash model with an explicit Max effort must be clamped: Flash
        // never accepts Max.
        let config = AgentConfig {
            model: DEEPSEEK_V4_FLASH.to_string(),
            reasoning_effort: ReasoningEffortSetting::Max,
            ..AgentConfig::default()
        };
        let route = resolve_turn_route(
            &config,
            &ModelRegistry::default(),
            "anything",
            false,
            RouteContext::default(),
        );
        assert_eq!(route.effective_model, DEEPSEEK_V4_FLASH);
        assert_eq!(route.effective_effort, ReasoningEffort::High);
    }

    #[test]
    fn debugging_unifies_to_pro_and_max() {
        // Unified table: a debugging prompt drives the strong model AND deep
        // reasoning, even when short (previously short → Flash regardless).
        let config = AgentConfig {
            model: AUTO_MODEL.to_string(),
            reasoning_effort: ReasoningEffortSetting::Auto,
            ..AgentConfig::default()
        };
        let route = resolve_turn_route(
            &config,
            &ModelRegistry::default(),
            "fix this error",
            false,
            RouteContext::default(),
        );
        assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
        assert_eq!(route.effective_effort, ReasoningEffort::Max);
    }

    #[test]
    fn context_pressure_forces_pro_without_keywords() {
        let config = AgentConfig {
            model: AUTO_MODEL.to_string(),
            reasoning_effort: ReasoningEffortSetting::Auto,
            ..AgentConfig::default()
        };
        let ctx = RouteContext {
            context_tokens: 800_000,
            context_window: 1_000_000,
        };
        let route = resolve_turn_route(&config, &ModelRegistry::default(), "hi", false, ctx);
        assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
        assert_eq!(route.source, RouteSource::HardRule);
    }

    #[test]
    fn api_fallback_only_for_auto_pro() {
        let route = TurnRoute {
            requested_model: "auto".to_string(),
            effective_model: DEEPSEEK_V4_PRO.to_string(),
            auto_model: true,
            reasoning_setting: ReasoningEffortSetting::High,
            effective_effort: ReasoningEffort::High,
            auto_effort: false,
            used_model_fallback: false,
            route_reason: "test".to_string(),
            fallback_reason: None,
            source: RouteSource::Heuristic,
        };
        assert_eq!(api_fallback_model(&route), Some(DEEPSEEK_V4_FLASH));
    }

    #[test]
    fn auto_model_returns_human_readable_reason() {
        let (model, reason) = select_auto_model_with_reason("please debug this error", false);
        assert_eq!(model, DEEPSEEK_V4_PRO);
        assert!(reason.contains("debug"));

        let (model, reason) = select_auto_model_with_reason("hi", false);
        assert_eq!(model, DEEPSEEK_V4_FLASH);
        assert!(reason.contains("短提示"));
    }

    #[test]
    fn fixed_pro_model_keeps_requested_id() {
        let config = AgentConfig {
            model: DEEPSEEK_V4_PRO.to_string(),
            reasoning_effort: ReasoningEffortSetting::High,
            cost_currency: CostCurrency::Cny,
            ..AgentConfig::default()
        };
        let route = resolve_turn_route(
            &config,
            &ModelRegistry::default(),
            "hello",
            false,
            RouteContext::default(),
        );
        assert!(!route.auto_model);
        assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
    }
}
