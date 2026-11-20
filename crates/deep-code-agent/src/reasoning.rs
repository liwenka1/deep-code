//! Reasoning-effort tiers for DeepSeek beta chat completions.

use serde::{Deserialize, Serialize};

/// Explicit thinking level. Configuration and model requests use the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortSetting {
    None,
    Low,
    #[default]
    High,
    Max,
}

impl ReasoningEffortSetting {
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
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
}
