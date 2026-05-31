//! Heuristic auto model selection for DeepSeek pro / flash routing.

use crate::config::AgentConfig;
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO, ModelRegistry};
use crate::reasoning::{ReasoningEffort, ReasoningEffortSetting};

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
) -> TurnRoute {
    let resolution = registry.resolve(Some(config.model.as_str()));
    let auto_model = resolution.resolved_id == AUTO_MODEL;
    let (effective_model, route_reason) = if auto_model {
        select_auto_model_with_reason(user_prompt, config.auto_cost_saving)
    } else {
        (
            resolution.resolved_id.clone(),
            format!("固定模型配置：{}", resolution.resolved_id),
        )
    };

    let auto_effort = config.reasoning_effort.is_auto();
    let effective_effort = config.reasoning_effort.resolve(is_subagent, user_prompt);

    TurnRoute {
        requested_model: config.model.clone(),
        effective_model,
        auto_model,
        reasoning_setting: config.reasoning_effort,
        effective_effort,
        auto_effort,
        used_model_fallback: resolution.used_fallback && !auto_model,
        route_reason,
        fallback_reason: None,
    }
}

/// Short prompts → Flash; complex keywords or long prompts → Pro.
#[must_use]
pub fn select_auto_model(input: &str, cost_saving: bool) -> String {
    select_auto_model_with_reason(input, cost_saving).0
}

/// Short prompts → Flash; complex keywords or long prompts → Pro, with a reason
/// suitable for status surfaces.
#[must_use]
pub fn select_auto_model_with_reason(input: &str, cost_saving: bool) -> (String, String) {
    let len = input.chars().count();
    let lower = input.to_lowercase();

    let borderline = [
        "implement",
        "analyze",
        "\u{5b9e}\u{73b0}",
        "\u{5206}\u{6790}",
    ];
    let strong_match = COMPLEX_KEYWORDS
        .iter()
        .find(|keyword| !borderline.contains(keyword) && lower.contains(**keyword));
    let borderline_match = borderline.iter().find(|keyword| lower.contains(**keyword));
    if let Some(keyword) = strong_match {
        return (
            DEEPSEEK_V4_PRO.to_string(),
            format!("命中复杂任务关键词“{keyword}”，使用 Pro 以获得更强推理和工具规划能力"),
        );
    }
    if !cost_saving && let Some(keyword) = borderline_match {
        return (
            DEEPSEEK_V4_PRO.to_string(),
            format!("任务包含“{keyword}”，且未开启成本优先，使用 Pro"),
        );
    }
    if len < 100 {
        return (
            DEEPSEEK_V4_FLASH.to_string(),
            "短提示优先使用 Flash，降低延迟和成本".to_string(),
        );
    }
    let long_threshold = if cost_saving { 1_000 } else { 500 };
    if len > long_threshold {
        return (
            DEEPSEEK_V4_PRO.to_string(),
            format!("输入长度 {len} 超过阈值 {long_threshold}，使用 Pro 处理长上下文"),
        );
    }
    (
        DEEPSEEK_V4_FLASH.to_string(),
        "未命中复杂任务规则，默认使用 Flash 保持响应速度和成本效率".to_string(),
    )
}

const COMPLEX_KEYWORDS: &[&str] = &[
    "refactor",
    "architecture",
    "design",
    "debug",
    "security",
    "review",
    "audit",
    "migrate",
    "optimize",
    "rewrite",
    "implement",
    "analyze",
    "\u{91cd}\u{6784}",
    "\u{67b6}\u{6784}",
    "\u{8bbe}\u{8ba1}",
    "\u{8c03}\u{8bd5}",
    "\u{5b89}\u{5168}",
    "\u{5ba1}\u{67e5}",
    "\u{5ba1}\u{8ba1}",
    "\u{8fc1}\u{79fb}",
    "\u{4f18}\u{5316}",
    "\u{91cd}\u{5199}",
    "\u{5b9e}\u{73b0}",
    "\u{5206}\u{6790}",
];

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
        let route = resolve_turn_route(&config, &ModelRegistry::default(), "debug crash", false);
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
        let route = resolve_turn_route(&config, &ModelRegistry::default(), "debug crash", true);
        assert_eq!(route.effective_effort, ReasoningEffort::Low);
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
        let route = resolve_turn_route(&config, &ModelRegistry::default(), "hello", false);
        assert!(!route.auto_model);
        assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
    }
}
