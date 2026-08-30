use super::*;
use crate::pricing::CostCurrency;

fn auto_model(input: &str) -> String {
    classify_model(input, &RouteContext::default(), false, Lang::Zh).0
}

#[test]
fn short_prompt_routes_to_flash() {
    assert_eq!(auto_model("hello"), DEEPSEEK_V4_FLASH);
}

#[test]
fn debug_routes_to_pro() {
    assert_eq!(auto_model("please debug this error"), DEEPSEEK_V4_PRO);
}

#[test]
fn chinese_refactor_routes_to_pro() {
    assert_eq!(
        auto_model("\u{5e2e}\u{6211}\u{91cd}\u{6784}\u{8fd9}\u{4e2a}\u{6a21}\u{5757}"),
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
        Lang::Zh,
    );
    assert!(route.auto_model);
    assert!(route.auto_effort);
    assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
    assert_eq!(route.effective_effort, ReasoningEffort::Max);
    assert!(route.route_reason.contains("debug"));
}

#[test]
fn fixed_model_routes_as_hard_rule_without_fallback_label() {
    let config = AgentConfig {
        model: DEEPSEEK_V4_PRO.to_string(),
        ..AgentConfig::default()
    };
    let route = resolve_turn_route(
        &config,
        &ModelRegistry::default(),
        "hello",
        false,
        RouteContext::default(),
        Lang::Zh,
    );
    assert_eq!(route.source, RouteSource::HardRule);
    assert!(!route.used_model_fallback);
    assert!(!route.label().contains("fallback"));
}

#[test]
fn passthrough_model_is_not_labelled_as_api_fallback() {
    let config = AgentConfig {
        model: "deepseek-v9-experimental".to_string(),
        ..AgentConfig::default()
    };
    let route = resolve_turn_route(
        &config,
        &ModelRegistry::default(),
        "hello",
        false,
        RouteContext::default(),
        Lang::Zh,
    );
    assert_eq!(route.effective_model, "deepseek-v9-experimental");
    assert!(
        !route.used_model_fallback,
        "registry passthrough must not masquerade as an API fallback"
    );
    assert!(!route.label().contains("fallback"));
    assert!(route.route_reason.contains("不在目录中"));
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
        Lang::Zh,
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
        Lang::Zh,
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
        Lang::Zh,
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
        escalated: false,
    };
    let route = resolve_turn_route(
        &config,
        &ModelRegistry::default(),
        "hi",
        false,
        ctx,
        Lang::Zh,
    );
    assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
    assert_eq!(route.source, RouteSource::HardRule);
}

#[test]
fn cascade_escalation_forces_pro_on_trivial_prompt() {
    // Once Flash has struggled this session, even a short trivial prompt
    // that would normally be Flash routes to Pro, tagged as Cascade.
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        reasoning_effort: ReasoningEffortSetting::Auto,
        ..AgentConfig::default()
    };
    let ctx = RouteContext {
        escalated: true,
        ..RouteContext::default()
    };
    let route = resolve_turn_route(
        &config,
        &ModelRegistry::default(),
        "hi",
        false,
        ctx,
        Lang::Zh,
    );
    assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
    assert_eq!(route.source, RouteSource::Cascade);
    assert!(route.route_reason.contains("级联升级"));
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
    let (model, reason, _) = classify_model(
        "please debug this error",
        &RouteContext::default(),
        false,
        Lang::Zh,
    );
    assert_eq!(model, DEEPSEEK_V4_PRO);
    assert!(reason.contains("debug"));

    let (model, reason, _) = classify_model("hi", &RouteContext::default(), false, Lang::Zh);
    assert_eq!(model, DEEPSEEK_V4_FLASH);
    assert!(reason.contains("Flash"));
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
        Lang::Zh,
    );
    assert!(!route.auto_model);
    assert_eq!(route.effective_model, DEEPSEEK_V4_PRO);
}
