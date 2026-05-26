//! Reasoning-effort tiers for DeepSeek beta chat completions.

use serde::{Deserialize, Serialize};

/// User-facing reasoning effort setting (includes Auto).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortSetting {
    Off,
    Low,
    Medium,
    #[default]
    High,
    Max,
    Auto,
}

/// Concrete tier sent to the provider API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffortSetting {
    #[must_use]
    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    #[must_use]
    pub fn resolve(self, is_subagent: bool, last_user_message: &str) -> ReasoningEffort {
        if self.is_auto() {
            return select_auto_effort(is_subagent, last_user_message);
        }
        match self {
            Self::Off => ReasoningEffort::Off,
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::Max => ReasoningEffort::Max,
            Self::Auto => unreachable!("handled above"),
        }
    }
}

impl ReasoningEffort {
    #[must_use]
    pub fn as_api_value(self) -> Option<&'static str> {
        match self {
            Self::Off => Some("off"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Max => Some("max"),
        }
    }

    #[must_use]
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "med",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// Adaptive tier selection for Auto reasoning effort.
#[must_use]
pub fn select_auto_effort(is_subagent: bool, last_msg: &str) -> ReasoningEffort {
    if is_subagent {
        return ReasoningEffort::Low;
    }

    let lower = last_msg.to_lowercase();

    if HIGH_EFFORT_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return ReasoningEffort::Max;
    }

    if LOW_EFFORT_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return ReasoningEffort::Low;
    }

    ReasoningEffort::High
}

const HIGH_EFFORT_KEYWORDS: &[&str] = &[
    "debug",
    "error",
    "\u{8c03}\u{8bd5}",
    "\u{9519}\u{8bef}",
    "\u{62a5}\u{9519}",
    "\u{51fa}\u{9519}",
    "\u{5d29}\u{6e83}",
];

const LOW_EFFORT_KEYWORDS: &[&str] = &[
    "search",
    "lookup",
    "\u{641c}\u{7d22}",
    "\u{67e5}\u{627e}",
    "\u{67e5}\u{8be2}",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_effort_keywords() {
        assert_eq!(
            select_auto_effort(false, "debug this crash"),
            ReasoningEffort::Max
        );
        assert_eq!(
            select_auto_effort(false, "\u{641c}\u{7d22}\u{6587}\u{4ef6}"),
            ReasoningEffort::Low
        );
        assert_eq!(select_auto_effort(true, "debug"), ReasoningEffort::Low);
        assert_eq!(
            select_auto_effort(false, "refactor module"),
            ReasoningEffort::High
        );
    }
}
