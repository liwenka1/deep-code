//! Reasoning-effort tiers for DeepSeek beta chat completions.

use serde::{Deserialize, Serialize};

/// User-facing reasoning effort setting (includes Auto).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortSetting {
    None,
    Low,
    #[default]
    High,
    Max,
    Auto,
}

/// Concrete tier sent to the provider API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
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
            Self::None => "none",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
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
            Self::None => ReasoningEffort::None,
            Self::Low => ReasoningEffort::Low,
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
            Self::None => Some("none"),
            Self::Low => Some("low"),
            Self::High => Some("high"),
            Self::Max => Some("max"),
        }
    }

    #[must_use]
    pub fn short_label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// Adaptive tier selection for Auto reasoning effort.
///
/// Derives from the same keyword table as model selection
/// ([`crate::task_class`]) so the two axes stay consistent: debugging-class
/// keywords get `Max`, lookups get `Low`, everything else `High`.
#[must_use]
pub fn select_auto_effort(is_subagent: bool, last_msg: &str) -> ReasoningEffort {
    use crate::task_class::{TaskWeight, classify_keyword};

    if is_subagent {
        return ReasoningEffort::Low;
    }

    match classify_keyword(last_msg).map(|hit| hit.0) {
        Some(TaskWeight::Deep) => ReasoningEffort::Max,
        Some(TaskWeight::Light) => ReasoningEffort::Low,
        _ => ReasoningEffort::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_setting` is the serde spelling and a `parse` fixpoint, so the config
    /// key, the wire and the diagnostics name an effort tier the same way.
    #[test]
    fn effort_setting_spelling_is_the_serde_spelling_and_parses_back() {
        for setting in [
            ReasoningEffortSetting::None,
            ReasoningEffortSetting::Low,
            ReasoningEffortSetting::High,
            ReasoningEffortSetting::Max,
            ReasoningEffortSetting::Auto,
        ] {
            assert_eq!(
                serde_json::to_value(setting).unwrap(),
                serde_json::Value::String(setting.as_setting().to_string()),
                "{setting:?}"
            );
            assert_eq!(
                ReasoningEffortSetting::parse(setting.as_setting()),
                Some(setting)
            );
        }
    }

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

    #[test]
    fn canonical_efforts_use_current_deepseek_wire_values() {
        assert_eq!(ReasoningEffort::None.as_api_value(), Some("none"));
        assert_eq!(ReasoningEffort::High.as_api_value(), Some("high"));
        assert_eq!(
            ReasoningEffortSetting::None.resolve(false, "anything"),
            ReasoningEffort::None
        );
        assert_eq!(ReasoningEffortSetting::parse("medium"), None);
        assert_eq!(ReasoningEffortSetting::parse("off"), None);
    }
}
