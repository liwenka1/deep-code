use std::env;
use std::time::Duration;

use crate::error::{AgentError, AgentResult};
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_PRO};
use crate::pricing::CostCurrency;
use crate::reasoning::ReasoningEffortSetting;

pub const DEFAULT_DEEPSEEK_MODEL: &str = DEEPSEEK_V4_PRO;
pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/beta";
pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
pub const MODEL_ENV: &str = "DEEP_CODE_MODEL";
pub const REASONING_EFFORT_ENV: &str = "DEEP_CODE_REASONING_EFFORT";
pub const COST_CURRENCY_ENV: &str = "DEEP_CODE_COST_CURRENCY";
pub const AUTO_COST_SAVING_ENV: &str = "DEEP_CODE_AUTO_COST_SAVING";
pub const COMPACTION_THRESHOLD_ENV: &str = "DEEP_CODE_COMPACTION_THRESHOLD";
pub const STREAM_MAX_RETRIES_ENV: &str = "DEEP_CODE_STREAM_MAX_RETRIES";
pub const STREAM_CHUNK_TIMEOUT_ENV: &str = "DEEP_CODE_STREAM_CHUNK_TIMEOUT_SECS";
pub const STREAM_TOTAL_TIMEOUT_ENV: &str = "DEEP_CODE_STREAM_TOTAL_TIMEOUT_SECS";
pub const STREAM_MAX_BYTES_ENV: &str = "DEEP_CODE_STREAM_MAX_BYTES";

pub const DEFAULT_STREAM_MAX_RETRIES: u32 = 3;
pub const DEFAULT_STREAM_CHUNK_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_STREAM_TOTAL_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_STREAM_MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: ReasoningEffortSetting,
    pub auto_cost_saving: bool,
    pub cost_currency: CostCurrency,
    /// Override compaction token threshold (for dev/testing).
    pub compaction_threshold: Option<u32>,
    pub timeout: Option<Duration>,
    /// Transparent stream retries before any content arrived.
    pub stream_max_retries: u32,
    /// Abort when no stream chunk arrives within this window.
    pub stream_chunk_timeout: Duration,
    /// Hard ceiling for one model stream from open to close.
    pub stream_total_timeout: Duration,
    /// Abort when cumulative streamed content exceeds this size.
    pub stream_max_bytes: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            api_key: env::var(DEEPSEEK_API_KEY_ENV)
                .ok()
                .filter(|key| !key.trim().is_empty()),
            base_url: DEFAULT_DEEPSEEK_BASE_URL.to_string(),
            model: env::var(MODEL_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEEPSEEK_V4_PRO.to_string()),
            reasoning_effort: env::var(REASONING_EFFORT_ENV)
                .ok()
                .and_then(|value| ReasoningEffortSetting::parse(&value))
                .unwrap_or(ReasoningEffortSetting::High),
            auto_cost_saving: env_bool(AUTO_COST_SAVING_ENV),
            cost_currency: env::var(COST_CURRENCY_ENV)
                .ok()
                .and_then(|value| CostCurrency::parse(&value))
                .unwrap_or(CostCurrency::Cny),
            compaction_threshold: env::var(COMPACTION_THRESHOLD_ENV)
                .ok()
                .and_then(|value| value.parse().ok()),
            timeout: Some(Duration::from_secs(60)),
            stream_max_retries: env_parse(STREAM_MAX_RETRIES_ENV)
                .unwrap_or(DEFAULT_STREAM_MAX_RETRIES),
            stream_chunk_timeout: Duration::from_secs(
                env_parse(STREAM_CHUNK_TIMEOUT_ENV).unwrap_or(DEFAULT_STREAM_CHUNK_TIMEOUT_SECS),
            ),
            stream_total_timeout: Duration::from_secs(
                env_parse(STREAM_TOTAL_TIMEOUT_ENV).unwrap_or(DEFAULT_STREAM_TOTAL_TIMEOUT_SECS),
            ),
            stream_max_bytes: env_parse(STREAM_MAX_BYTES_ENV).unwrap_or(DEFAULT_STREAM_MAX_BYTES),
        }
    }
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    env::var(name).ok().and_then(|value| value.parse().ok())
}

impl AgentConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn auto_model_enabled(&self) -> bool {
        self.model.trim().eq_ignore_ascii_case(AUTO_MODEL)
    }

    pub fn require_api_key(&self) -> AgentResult<&str> {
        self.api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or(AgentError::MissingApiKey)
    }

    #[must_use]
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    #[must_use]
    pub fn uses_beta_endpoint(&self) -> bool {
        self.base_url.contains("/beta")
    }
}

fn env_bool(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_deepseek_defaults() {
        let config = AgentConfig {
            api_key: None,
            ..AgentConfig::default()
        };

        assert_eq!(config.base_url, DEFAULT_DEEPSEEK_BASE_URL);
        assert_eq!(config.model, DEEPSEEK_V4_PRO);
        assert_eq!(config.cost_currency, CostCurrency::Cny);
        assert_eq!(
            config.chat_completions_url(),
            "https://api.deepseek.com/beta/chat/completions"
        );
    }

    #[test]
    fn require_api_key_rejects_missing_key() {
        let config = AgentConfig {
            api_key: None,
            ..AgentConfig::default()
        };

        assert!(matches!(
            config.require_api_key(),
            Err(AgentError::MissingApiKey)
        ));
    }

    #[test]
    fn auto_model_flag() {
        let config = AgentConfig {
            model: AUTO_MODEL.to_string(),
            ..AgentConfig::default()
        };
        assert!(config.auto_model_enabled());
        let fixed = AgentConfig {
            model: DEEPSEEK_V4_PRO.to_string(),
            ..AgentConfig::default()
        };
        assert!(!fixed.auto_model_enabled());
    }
}
