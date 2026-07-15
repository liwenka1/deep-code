//! Agent configuration: the [`AgentConfig`] type and its built-in/env
//! defaults live here; the layered file loading lives in [`layers`].

mod layers;
mod write;

use std::env;
use std::time::Duration;

pub use layers::{
    ConfigLayer, ConfigLayerStatus, ConfigLoadReport, ConfigSources, LoadedAgentConfig,
};
pub use write::{GlobalConfigUpdate, validate_api_key, write_global_config_update};

use crate::error::{AgentError, AgentResult};
use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_PRO};
use crate::pricing::CostCurrency;
use crate::reasoning::ReasoningEffortSetting;

pub const DEFAULT_DEEPSEEK_MODEL: &str = DEEPSEEK_V4_PRO;
pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/beta";
pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Provider/runtime secrets that live in the parent process environment (the
/// LLM client reads the API key at startup; the HTTP server reads the auth
/// token on bind) but that NO spawned subprocess — shell, job, MCP, or LSP —
/// needs. Stripping them before spawn keeps an injected or third-party
/// subprocess from lifting a key straight out of its own environment.
///
/// `DEEP_CODE_RUNTIME_TOKEN` is duplicated from
/// `deep_code_runtime::auth::RUNTIME_TOKEN_ENV`; that crate depends on this
/// one, so the constant can't be imported here — keep the two in sync.
pub const SUBPROCESS_SECRET_ENV: &[&str] = &[DEEPSEEK_API_KEY_ENV, "DEEP_CODE_RUNTIME_TOKEN"];

pub const MODEL_ENV: &str = "DEEP_CODE_MODEL";
pub const REASONING_EFFORT_ENV: &str = "DEEP_CODE_REASONING_EFFORT";
pub const COST_CURRENCY_ENV: &str = "DEEP_CODE_COST_CURRENCY";
pub const AUTO_COST_SAVING_ENV: &str = "DEEP_CODE_AUTO_COST_SAVING";
pub const COMPACTION_THRESHOLD_ENV: &str = "DEEP_CODE_COMPACTION_THRESHOLD";
pub const STREAM_MAX_RETRIES_ENV: &str = "DEEP_CODE_STREAM_MAX_RETRIES";
pub const STREAM_CHUNK_TIMEOUT_ENV: &str = "DEEP_CODE_STREAM_CHUNK_TIMEOUT_SECS";
pub const STREAM_TOTAL_TIMEOUT_ENV: &str = "DEEP_CODE_STREAM_TOTAL_TIMEOUT_SECS";
pub const STREAM_MAX_BYTES_ENV: &str = "DEEP_CODE_STREAM_MAX_BYTES";
pub const APPROVAL_AUTO_ALLOW_ENV: &str = "DEEP_CODE_APPROVAL_AUTO_ALLOW";
pub const CHECKPOINT_MAX_SNAPSHOTS_ENV: &str = "DEEP_CODE_CHECKPOINT_MAX_SNAPSHOTS";

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
    /// Open-phase timeout: connect + request + response headers. Never bounds
    /// the streaming body — long generations legitimately exceed any fixed
    /// request timeout; stream liveness is enforced by the guards below.
    pub timeout: Option<Duration>,
    /// Transparent stream retries before any content arrived.
    pub stream_max_retries: u32,
    /// Abort when no stream chunk arrives within this window.
    pub stream_chunk_timeout: Duration,
    /// Hard ceiling for one model stream from open to close.
    pub stream_total_timeout: Duration,
    /// Abort when cumulative streamed content exceeds this size.
    pub stream_max_bytes: u64,
    /// Tool-name prefixes the user pre-approved: gated calls matching one of
    /// these run without prompting. Only env and the global config file may
    /// set this — project files are ignored (a repo must not disarm gates).
    pub approval_auto_allow: Vec<String>,
    /// Checkpoint retention cap: oldest snapshots beyond this count are
    /// pruned after each new snapshot (0 disables pruning).
    pub checkpoint_max_snapshots: usize,
}

impl Default for AgentConfig {
    /// Built-in defaults plus the environment overlay. Files are NOT read
    /// here; use [`AgentConfig::load`] for the full layered configuration.
    fn default() -> Self {
        let mut config = Self::builtin();
        let mut sources = ConfigSources::default();
        layers::apply_env_overlay(&mut config, &mut sources, &|name| env::var(name).ok());
        config
    }
}

impl AgentConfig {
    /// Pure built-in defaults: no environment, no files.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            api_key: None,
            base_url: DEFAULT_DEEPSEEK_BASE_URL.to_string(),
            model: DEEPSEEK_V4_PRO.to_string(),
            reasoning_effort: ReasoningEffortSetting::High,
            auto_cost_saving: false,
            cost_currency: CostCurrency::Cny,
            compaction_threshold: None,
            timeout: Some(Duration::from_secs(60)),
            stream_max_retries: DEFAULT_STREAM_MAX_RETRIES,
            stream_chunk_timeout: Duration::from_secs(DEFAULT_STREAM_CHUNK_TIMEOUT_SECS),
            stream_total_timeout: Duration::from_secs(DEFAULT_STREAM_TOTAL_TIMEOUT_SECS),
            stream_max_bytes: DEFAULT_STREAM_MAX_BYTES,
            approval_auto_allow: Vec::new(),
            checkpoint_max_snapshots: crate::checkpoint::DEFAULT_MAX_SNAPSHOTS,
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_config_uses_deepseek_defaults() {
        // builtin() (not default()) so developer env vars cannot flake this.
        let config = AgentConfig::builtin();

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
        let config = AgentConfig::builtin();

        assert!(matches!(
            config.require_api_key(),
            Err(AgentError::MissingApiKey)
        ));
    }

    #[test]
    fn auto_model_flag() {
        let config = AgentConfig {
            model: AUTO_MODEL.to_string(),
            ..AgentConfig::builtin()
        };
        assert!(config.auto_model_enabled());
        let fixed = AgentConfig {
            model: DEEPSEEK_V4_PRO.to_string(),
            ..AgentConfig::builtin()
        };
        assert!(!fixed.auto_model_enabled());
    }
}
