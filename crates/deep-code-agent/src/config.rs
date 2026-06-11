use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
    /// Built-in defaults plus the environment overlay. Files are NOT read
    /// here; use [`AgentConfig::load`] for the full layered configuration.
    fn default() -> Self {
        let mut config = Self::builtin();
        let mut sources = ConfigSources::default();
        apply_env_overlay(&mut config, &mut sources, &|name| env::var(name).ok());
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
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        Self::default()
    }

    /// Load the layered configuration for a workspace:
    /// builtin → global `~/.deep-code/config.toml` → project
    /// `<workspace>/.deep-code/config.toml` (whitelisted) → environment.
    /// CLI flags are the caller's responsibility, applied after this.
    ///
    /// Never fails: unreadable or invalid layers are skipped with a warning
    /// in the returned report.
    #[must_use]
    pub fn load(workspace: &Path) -> LoadedAgentConfig {
        let global = home_dir().map(|home| home.join(".deep-code").join("config.toml"));
        let project = Some(workspace.join(".deep-code").join("config.toml"));
        Self::load_with(global, project, &|name| env::var(name).ok())
    }

    /// Layered load with explicit file paths and environment lookup.
    /// Test seam for [`AgentConfig::load`]; same semantics.
    #[must_use]
    pub fn load_with(
        global: Option<PathBuf>,
        project: Option<PathBuf>,
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> LoadedAgentConfig {
        let mut config = Self::builtin();
        let mut report = ConfigLoadReport::default();

        for (layer, path) in [
            (ConfigLayer::Global, global),
            (ConfigLayer::Project, project),
        ] {
            let Some(path) = path else { continue };
            match read_config_file(&path) {
                FileRead::Missing => report.layers.push(ConfigLayerStatus {
                    name: layer.label(),
                    path: path.display().to_string(),
                    present: false,
                    error: None,
                }),
                FileRead::Error(message) => {
                    report.warnings.push(format!(
                        "配置文件 {} 无法使用，已跳过该层：{message}",
                        path.display()
                    ));
                    report.layers.push(ConfigLayerStatus {
                        name: layer.label(),
                        path: path.display().to_string(),
                        present: true,
                        error: Some(message),
                    });
                }
                FileRead::Parsed(file) => {
                    report.layers.push(ConfigLayerStatus {
                        name: layer.label(),
                        path: path.display().to_string(),
                        present: true,
                        error: None,
                    });
                    apply_file_overlay(&mut config, &file, layer, &mut report);
                    if layer == ConfigLayer::Global
                        && file
                            .provider
                            .api_key
                            .as_deref()
                            .is_some_and(|key| !key.trim().is_empty())
                    {
                        check_global_key_permissions(&path, &mut report.warnings);
                    }
                }
            }
        }

        apply_env_overlay(&mut config, &mut report.sources, env_lookup);
        LoadedAgentConfig { config, report }
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

/// Configuration layer, ordered from weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLayer {
    Builtin,
    Global,
    Project,
    Env,
}

impl ConfigLayer {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "default",
            Self::Global => "global",
            Self::Project => "project",
            Self::Env => "env",
        }
    }
}

/// Which layer last set each key field — lets doctor explain "当前 model
/// 是哪一层给的".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConfigSources {
    pub api_key: ConfigLayer,
    pub base_url: ConfigLayer,
    pub model: ConfigLayer,
    pub reasoning_effort: ConfigLayer,
    pub cost_currency: ConfigLayer,
}

impl Default for ConfigSources {
    fn default() -> Self {
        Self {
            api_key: ConfigLayer::Builtin,
            base_url: ConfigLayer::Builtin,
            model: ConfigLayer::Builtin,
            reasoning_effort: ConfigLayer::Builtin,
            cost_currency: ConfigLayer::Builtin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigLayerStatus {
    pub name: &'static str,
    pub path: String,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ConfigLoadReport {
    pub layers: Vec<ConfigLayerStatus>,
    pub warnings: Vec<String>,
    pub sources: ConfigSources,
}

/// Result of [`AgentConfig::load`]: the effective config plus how it was
/// assembled.
#[derive(Debug, Clone)]
pub struct LoadedAgentConfig {
    pub config: AgentConfig,
    pub report: ConfigLoadReport,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    provider: ProviderSection,
    cost: CostSection,
    context: ContextSection,
    stream: StreamSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProviderSection {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CostSection {
    currency: Option<String>,
    auto_cost_saving: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContextSection {
    compaction_threshold: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StreamSection {
    max_retries: Option<u32>,
    chunk_timeout_secs: Option<u64>,
    total_timeout_secs: Option<u64>,
    max_bytes: Option<u64>,
}

enum FileRead {
    Missing,
    Error(String),
    Parsed(ConfigFile),
}

fn read_config_file(path: &Path) -> FileRead {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return FileRead::Missing,
        Err(error) => return FileRead::Error(format!("读取失败：{error}")),
    };
    match toml::from_str::<ConfigFile>(&raw) {
        Ok(file) => FileRead::Parsed(file),
        Err(error) => FileRead::Error(format!("TOML 解析失败：{error}")),
    }
}

fn apply_file_overlay(
    config: &mut AgentConfig,
    file: &ConfigFile,
    layer: ConfigLayer,
    report: &mut ConfigLoadReport,
) {
    let project = layer == ConfigLayer::Project;

    if let Some(api_key) = file
        .provider
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
    {
        if project {
            report.warnings.push(
                "项目配置中的 provider.api_key 已忽略：密钥只能来自环境变量或全局配置，避免随仓库泄露或被恶意仓库注入".to_string(),
            );
        } else {
            config.api_key = Some(api_key.to_string());
            report.sources.api_key = layer;
        }
    }

    if let Some(base_url) = file
        .provider
        .base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        if project {
            report.warnings.push(format!(
                "警告：项目配置把 base_url 覆盖为 {base_url}。请确认该仓库可信——恶意端点可以拿到你的 API Key 和全部上下文"
            ));
        }
        config.base_url = base_url.to_string();
        report.sources.base_url = layer;
    }

    if let Some(model) = file
        .provider
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        config.model = model.to_string();
        report.sources.model = layer;
    }

    if let Some(value) = file.provider.reasoning_effort.as_deref() {
        if let Some(effort) = ReasoningEffortSetting::parse(value) {
            config.reasoning_effort = effort;
            report.sources.reasoning_effort = layer;
        } else {
            report.warnings.push(format!(
                "{} 配置的 provider.reasoning_effort='{value}' 无法识别，已忽略",
                layer.label()
            ));
        }
    }

    if let Some(secs) = file.provider.timeout_secs {
        if project {
            report.warnings.push(
                "项目配置中的 provider.timeout_secs 不在白名单内，已忽略".to_string(),
            );
        } else {
            config.timeout = Some(Duration::from_secs(secs));
        }
    }

    if let Some(value) = file.cost.currency.as_deref() {
        if let Some(currency) = CostCurrency::parse(value) {
            config.cost_currency = currency;
            report.sources.cost_currency = layer;
        } else {
            report.warnings.push(format!(
                "{} 配置的 cost.currency='{value}' 无法识别，已忽略",
                layer.label()
            ));
        }
    }
    if let Some(value) = file.cost.auto_cost_saving {
        config.auto_cost_saving = value;
    }

    if let Some(value) = file.context.compaction_threshold {
        config.compaction_threshold = Some(value);
    }

    if let Some(value) = file.stream.max_retries {
        config.stream_max_retries = value;
    }
    if let Some(value) = file.stream.chunk_timeout_secs {
        config.stream_chunk_timeout = Duration::from_secs(value);
    }
    if let Some(value) = file.stream.total_timeout_secs {
        config.stream_total_timeout = Duration::from_secs(value);
    }
    if let Some(value) = file.stream.max_bytes {
        config.stream_max_bytes = value;
    }
}

fn apply_env_overlay(
    config: &mut AgentConfig,
    sources: &mut ConfigSources,
    lookup: &dyn Fn(&str) -> Option<String>,
) {
    if let Some(key) = lookup(DEEPSEEK_API_KEY_ENV).filter(|value| !value.trim().is_empty()) {
        config.api_key = Some(key);
        sources.api_key = ConfigLayer::Env;
    }
    if let Some(model) = lookup(MODEL_ENV).filter(|value| !value.trim().is_empty()) {
        config.model = model;
        sources.model = ConfigLayer::Env;
    }
    if let Some(effort) =
        lookup(REASONING_EFFORT_ENV).and_then(|value| ReasoningEffortSetting::parse(&value))
    {
        config.reasoning_effort = effort;
        sources.reasoning_effort = ConfigLayer::Env;
    }
    if let Some(value) = lookup(AUTO_COST_SAVING_ENV) {
        config.auto_cost_saving = matches!(value.trim(), "1" | "true" | "yes" | "on");
    }
    if let Some(currency) = lookup(COST_CURRENCY_ENV).and_then(|value| CostCurrency::parse(&value))
    {
        config.cost_currency = currency;
        sources.cost_currency = ConfigLayer::Env;
    }
    if let Some(value) = lookup(COMPACTION_THRESHOLD_ENV).and_then(|value| value.parse().ok()) {
        config.compaction_threshold = Some(value);
    }
    if let Some(value) = lookup(STREAM_MAX_RETRIES_ENV).and_then(|value| value.parse().ok()) {
        config.stream_max_retries = value;
    }
    if let Some(value) = lookup(STREAM_CHUNK_TIMEOUT_ENV).and_then(|value| value.parse().ok()) {
        config.stream_chunk_timeout = Duration::from_secs(value);
    }
    if let Some(value) = lookup(STREAM_TOTAL_TIMEOUT_ENV).and_then(|value| value.parse().ok()) {
        config.stream_total_timeout = Duration::from_secs(value);
    }
    if let Some(value) = lookup(STREAM_MAX_BYTES_ENV).and_then(|value| value.parse().ok()) {
        config.stream_max_bytes = value;
    }
}

#[cfg(unix)]
fn check_global_key_permissions(path: &Path, warnings: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            warnings.push(format!(
                "全局配置 {} 含 api_key 但对组/其他用户可读，建议执行 chmod 600",
                path.display()
            ));
        }
    }
}

#[cfg(not(unix))]
fn check_global_key_permissions(_path: &Path, _warnings: &mut Vec<String>) {}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
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

    fn no_env(_name: &str) -> Option<String> {
        None
    }

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn load_without_files_or_env_equals_builtin() {
        let loaded = AgentConfig::load_with(None, None, &no_env);
        assert_eq!(loaded.config, AgentConfig::builtin());
        assert!(loaded.report.warnings.is_empty());
        assert_eq!(loaded.report.sources.model, ConfigLayer::Builtin);
    }

    #[test]
    fn layered_load_respects_precedence() {
        let global_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let global = write_config(
            global_dir.path(),
            "[provider]\nmodel = \"global-model\"\nbase_url = \"https://global.example\"\n[cost]\ncurrency = \"usd\"\n",
        );
        let project = write_config(project_dir.path(), "[provider]\nmodel = \"project-model\"\n");

        // global < project for model; env wins over both.
        let env = |name: &str| {
            (name == MODEL_ENV).then(|| "env-model".to_string())
        };
        let loaded = AgentConfig::load_with(Some(global.clone()), Some(project.clone()), &env);
        assert_eq!(loaded.config.model, "env-model");
        assert_eq!(loaded.report.sources.model, ConfigLayer::Env);
        assert_eq!(loaded.config.base_url, "https://global.example");
        assert_eq!(loaded.report.sources.base_url, ConfigLayer::Global);
        assert_eq!(loaded.config.cost_currency, CostCurrency::Usd);

        // Without env, project wins over global.
        let loaded = AgentConfig::load_with(Some(global.clone()), Some(project), &no_env);
        assert_eq!(loaded.config.model, "project-model");
        assert_eq!(loaded.report.sources.model, ConfigLayer::Project);

        // Without project, global wins.
        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert_eq!(loaded.config.model, "global-model");
        assert_eq!(loaded.report.sources.model, ConfigLayer::Global);
    }

    #[test]
    fn project_layer_rejects_api_key_and_warns_on_base_url() {
        let project_dir = tempfile::tempdir().unwrap();
        let project = write_config(
            project_dir.path(),
            "[provider]\napi_key = \"sk-injected\"\nbase_url = \"https://evil.example\"\ntimeout_secs = 5\n",
        );

        let loaded = AgentConfig::load_with(None, Some(project), &no_env);
        assert_eq!(loaded.config.api_key, None, "project api_key must be ignored");
        assert_eq!(loaded.config.base_url, "https://evil.example");
        assert_eq!(
            loaded.config.timeout,
            AgentConfig::builtin().timeout,
            "project timeout_secs is outside the whitelist"
        );
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("api_key"))
        );
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("base_url"))
        );
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("timeout_secs"))
        );
    }

    #[test]
    fn global_layer_applies_api_key_and_stream_tuning() {
        let global_dir = tempfile::tempdir().unwrap();
        let global = write_config(
            global_dir.path(),
            "[provider]\napi_key = \"sk-global\"\ntimeout_secs = 120\n[stream]\nmax_retries = 7\nchunk_timeout_secs = 30\n",
        );

        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert_eq!(loaded.config.api_key.as_deref(), Some("sk-global"));
        assert_eq!(loaded.report.sources.api_key, ConfigLayer::Global);
        assert_eq!(loaded.config.timeout, Some(Duration::from_secs(120)));
        assert_eq!(loaded.config.stream_max_retries, 7);
        assert_eq!(
            loaded.config.stream_chunk_timeout,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn invalid_toml_layer_is_skipped_with_warning_not_panic() {
        let global_dir = tempfile::tempdir().unwrap();
        let global = write_config(global_dir.path(), "[provider\nmodel = broken");

        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert_eq!(loaded.config, AgentConfig::builtin());
        let layer = loaded
            .report
            .layers
            .iter()
            .find(|layer| layer.name == "global")
            .expect("global layer status");
        assert!(layer.present);
        assert!(layer.error.as_deref().is_some_and(|error| error.contains("解析失败")));
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("已跳过该层"))
        );
    }

    #[test]
    fn missing_files_are_reported_as_absent_layers() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = AgentConfig::load_with(
            Some(dir.path().join("nope/config.toml")),
            Some(dir.path().join("also-nope/config.toml")),
            &no_env,
        );
        assert_eq!(loaded.report.layers.len(), 2);
        assert!(loaded.report.layers.iter().all(|layer| !layer.present));
        assert!(loaded.report.warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn world_readable_global_key_file_warns_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let global_dir = tempfile::tempdir().unwrap();
        let global = write_config(global_dir.path(), "[provider]\napi_key = \"sk-global\"\n");
        fs::set_permissions(&global, fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = AgentConfig::load_with(Some(global.clone()), None, &no_env);
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("chmod 600"))
        );

        fs::set_permissions(&global, fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .all(|warning| !warning.contains("chmod 600"))
        );
    }
}
