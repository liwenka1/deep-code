use std::env;
use std::time::Duration;

use crate::error::{AgentError, AgentResult};

pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/beta";
pub const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-pro";
pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub timeout: Option<Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            api_key: env::var(DEEPSEEK_API_KEY_ENV)
                .ok()
                .filter(|key| !key.trim().is_empty()),
            base_url: DEFAULT_DEEPSEEK_BASE_URL.to_string(),
            model: DEFAULT_DEEPSEEK_MODEL.to_string(),
            timeout: Some(Duration::from_secs(60)),
        }
    }
}

impl AgentConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self::default()
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
        assert_eq!(config.model, DEFAULT_DEEPSEEK_MODEL);
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
}
