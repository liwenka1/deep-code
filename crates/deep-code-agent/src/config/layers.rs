//! Layered configuration assembly: builtin → global TOML → project TOML
//! (whitelisted) → environment. The parent module owns [`AgentConfig`]
//! itself; this module owns how it is produced from files and env.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    APPROVAL_AUTO_ALLOW_ENV, AUTO_COST_SAVING_ENV, AgentConfig, CHECKPOINT_MAX_SNAPSHOTS_ENV,
    COMPACTION_THRESHOLD_ENV, COST_CURRENCY_ENV, DEEPSEEK_API_KEY_ENV, LANG_ENV, MODEL_ENV,
    REASONING_EFFORT_ENV, STREAM_CHUNK_TIMEOUT_ENV, STREAM_MAX_BYTES_ENV, STREAM_MAX_RETRIES_ENV,
    STREAM_TOTAL_TIMEOUT_ENV,
};
use crate::execution_policy::PermissionMode;
use crate::i18n::{Lang, TextId, tr_with};
use crate::paths::home_dir;
use crate::pricing::CostCurrency;
use crate::reasoning::ReasoningEffortSetting;

/// A config warning captured during load as `(key, params)` and rendered into
/// the user's language only at the end — the language itself comes from the
/// config being assembled, so it isn't known until every layer is applied.
type PendingWarning = (TextId, Vec<(&'static str, String)>);

fn render_warning(lang: Lang, (id, args): &PendingWarning) -> String {
    let refs: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
    tr_with(lang, *id, &refs)
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

impl AgentConfig {
    /// Load the layered configuration for a workspace:
    /// builtin → global `~/.deep-code/config.toml` → project
    /// `<workspace>/.deep-code/config.toml` (whitelisted) → environment.
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
        let mut pending: Vec<PendingWarning> = Vec::new();

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
                    pending.push((
                        TextId::CfgFileUnusable,
                        vec![
                            ("path", path.display().to_string()),
                            ("detail", message.clone()),
                        ],
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
                    apply_file_overlay(&mut config, &file, layer, &mut report, &mut pending);
                    if layer == ConfigLayer::Global
                        && file
                            .provider
                            .api_key
                            .as_deref()
                            .is_some_and(|key| !key.trim().is_empty())
                    {
                        check_global_key_permissions(&path, &mut pending);
                    }
                }
            }
        }

        apply_env_overlay(&mut config, &mut report.sources, env_lookup);

        // Render deferred warnings now that the final language is known.
        // Resolve through the same `env_lookup` seam so tests stay deterministic.
        let lang = Lang::resolve(&config.language, env_lookup);
        report.warnings = pending
            .iter()
            .map(|warning| render_warning(lang, warning))
            .collect();
        LoadedAgentConfig { config, report }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    provider: ProviderSection,
    cost: CostSection,
    context: ContextSection,
    stream: StreamSection,
    approval: ApprovalSection,
    checkpoints: CheckpointsSection,
    ui: UiSection,
    lsp: LspSection,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ApprovalSection {
    auto_allow: Option<Vec<String>>,
    default_mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CheckpointsSection {
    max_snapshots: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UiSection {
    language: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LspSection {
    enabled: Option<bool>,
}

enum FileRead {
    Missing,
    Error(String),
    Parsed(Box<ConfigFile>),
}

fn read_config_file(path: &Path) -> FileRead {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return FileRead::Missing,
        // Carry the raw underlying error; the localized "file unusable" wrapper
        // is applied at the warning site (the detail is inherently English).
        Err(error) => return FileRead::Error(error.to_string()),
    };
    match toml::from_str::<ConfigFile>(&raw) {
        Ok(file) => FileRead::Parsed(Box::new(file)),
        Err(error) => FileRead::Error(error.to_string()),
    }
}

fn apply_file_overlay(
    config: &mut AgentConfig,
    file: &ConfigFile,
    layer: ConfigLayer,
    report: &mut ConfigLoadReport,
    pending: &mut Vec<PendingWarning>,
) {
    let project = layer == ConfigLayer::Project;

    if let Some(api_key) = file
        .provider
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
    {
        if project {
            pending.push((TextId::CfgProjectApiKeyIgnored, Vec::new()));
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
            pending.push((
                TextId::CfgProjectBaseUrlOverride,
                vec![("url", base_url.to_string())],
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
            pending.push((
                TextId::CfgUnknownReasoning,
                vec![
                    ("layer", layer.label().to_string()),
                    ("value", value.to_string()),
                ],
            ));
        }
    }

    if let Some(secs) = file.provider.timeout_secs {
        if project {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![("field", "provider.timeout_secs".to_string())],
            ));
        } else {
            config.timeout = Some(Duration::from_secs(secs));
        }
    }

    if let Some(value) = file.cost.currency.as_deref() {
        if let Some(currency) = CostCurrency::parse(value) {
            config.cost_currency = currency;
            report.sources.cost_currency = layer;
        } else {
            pending.push((
                TextId::CfgUnknownCurrency,
                vec![
                    ("layer", layer.label().to_string()),
                    ("value", value.to_string()),
                ],
            ));
        }
    }
    // Runtime-behavior knobs below share one rule with provider.timeout_secs:
    // not project-configurable. A repo's config must not be able to starve
    // streams, blow up snapshot retention, or flip cost/compaction behavior —
    // set these globally or via environment instead.
    let mut reject_project = |field: &str| {
        pending.push((
            TextId::CfgProjectFieldIgnored,
            vec![("field", field.to_string())],
        ));
    };
    if let Some(value) = file.cost.auto_cost_saving {
        if project {
            reject_project("cost.auto_cost_saving");
        } else {
            config.auto_cost_saving = value;
        }
    }

    if let Some(value) = file.context.compaction_threshold {
        if project {
            reject_project("context.compaction_threshold");
        } else {
            config.compaction_threshold = Some(value);
        }
    }

    if let Some(value) = file.stream.max_retries {
        if project {
            reject_project("stream.max_retries");
        } else {
            config.stream_max_retries = value;
        }
    }
    if let Some(value) = file.stream.chunk_timeout_secs {
        if project {
            reject_project("stream.chunk_timeout_secs");
        } else {
            config.stream_chunk_timeout = Duration::from_secs(value);
        }
    }
    if let Some(value) = file.stream.total_timeout_secs {
        if project {
            reject_project("stream.total_timeout_secs");
        } else {
            config.stream_total_timeout = Duration::from_secs(value);
        }
    }
    if let Some(value) = file.stream.max_bytes {
        if project {
            reject_project("stream.max_bytes");
        } else {
            config.stream_max_bytes = value;
        }
    }
    if let Some(value) = file.checkpoints.max_snapshots {
        if project {
            reject_project("checkpoints.max_snapshots");
        } else {
            config.checkpoint_max_snapshots = value;
        }
    }

    // UI preference: harmless from any layer, so the project file may set it
    // (a repo declaring its team's display language is fine).
    if let Some(language) = file
        .ui
        .language
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        config.language = language.trim().to_string();
    }

    // Diagnostics preference: turning LSP off is harmless from any layer (a
    // repo can only reduce what runs, never widen access).
    if let Some(enabled) = file.lsp.enabled {
        config.lsp_enabled = enabled;
    }

    if let Some(rules) = &file.approval.auto_allow {
        if project {
            pending.push((TextId::CfgProjectAutoAllowIgnored, Vec::new()));
        } else {
            config.approval_auto_allow = rules
                .iter()
                .map(|rule| rule.trim().to_string())
                .filter(|rule| !rule.is_empty())
                .collect();
        }
    }

    if let Some(mode) = file
        .approval
        .default_mode
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .and_then(PermissionMode::parse)
    {
        // A project file may pick default/accept-edits, but must NOT be able to
        // launch you into auto/yolo — a malicious repo mustn't silently disarm
        // the approval gate. Unknown values simply degrade to the default.
        if project && matches!(mode, PermissionMode::Auto | PermissionMode::Yolo) {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![("field", "approval.default_mode=auto/yolo".to_string())],
            ));
        } else {
            config.default_permission_mode = mode;
        }
    }
}

pub(super) fn apply_env_overlay(
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
    if let Some(value) = lookup(LANG_ENV).filter(|value| !value.trim().is_empty()) {
        config.language = value.trim().to_string();
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
    if let Some(value) = lookup(CHECKPOINT_MAX_SNAPSHOTS_ENV).and_then(|value| value.parse().ok()) {
        config.checkpoint_max_snapshots = value;
    }
    if let Some(value) = lookup(APPROVAL_AUTO_ALLOW_ENV) {
        config.approval_auto_allow = value
            .split(',')
            .map(|rule| rule.trim().to_string())
            .filter(|rule| !rule.is_empty())
            .collect();
    }
}

#[cfg(unix)]
fn check_global_key_permissions(path: &Path, pending: &mut Vec<PendingWarning>) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            pending.push((
                TextId::CfgGlobalKeyPerms,
                vec![("path", path.display().to_string())],
            ));
        }
    }
}

#[cfg(not(unix))]
fn check_global_key_permissions(_path: &Path, _pending: &mut Vec<PendingWarning>) {}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn language_layers_file_then_env_default_auto() {
        // Builtin default.
        assert_eq!(AgentConfig::builtin().language, "auto");

        // File layers (project overrides global; blank is ignored).
        let global_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let global = write_config(global_dir.path(), "[ui]\nlanguage = \"en\"\n");
        let project = write_config(project_dir.path(), "[ui]\nlanguage = \" zh \"\n");
        let loaded = AgentConfig::load_with(Some(global.clone()), Some(project), &no_env);
        assert_eq!(loaded.config.language, "zh");

        // Env wins over files.
        let env = |name: &str| (name == LANG_ENV).then(|| "en".to_string());
        let loaded = AgentConfig::load_with(Some(global), None, &env);
        assert_eq!(loaded.config.language, "en");
    }

    #[test]
    fn lsp_enabled_defaults_on_and_any_file_layer_may_turn_it_off() {
        assert!(AgentConfig::builtin().lsp_enabled);

        let global_dir = tempfile::tempdir().unwrap();
        let global = write_config(global_dir.path(), "[lsp]\nenabled = false\n");
        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert!(!loaded.config.lsp_enabled);

        let project_dir = tempfile::tempdir().unwrap();
        let project = write_config(project_dir.path(), "[lsp]\nenabled = false\n");
        let loaded = AgentConfig::load_with(None, Some(project), &no_env);
        assert!(!loaded.config.lsp_enabled, "project layer may reduce");
    }

    #[test]
    fn default_permission_mode_layers_and_project_cannot_set_auto_or_yolo() {
        use crate::execution_policy::PermissionMode;
        let dir = tempfile::tempdir().unwrap();

        // Global config may set any mode, including yolo.
        let global = write_config(dir.path(), "[approval]\ndefault_mode = \"yolo\"\n");
        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert_eq!(loaded.config.default_permission_mode, PermissionMode::Yolo);

        // Project config may set accept-edits...
        let project_dir = tempfile::tempdir().unwrap();
        let project = write_config(
            project_dir.path(),
            "[approval]\ndefault_mode = \"accept_edits\"\n",
        );
        let loaded = AgentConfig::load_with(None, Some(project), &no_env);
        assert_eq!(
            loaded.config.default_permission_mode,
            PermissionMode::AcceptEdits
        );

        // ...but NOT auto/yolo — a repo mustn't disarm the gate; capped + warned.
        let evil_dir = tempfile::tempdir().unwrap();
        let evil = write_config(evil_dir.path(), "[approval]\ndefault_mode = \"yolo\"\n");
        let loaded = AgentConfig::load_with(None, Some(evil), &no_env);
        assert_eq!(
            loaded.config.default_permission_mode,
            PermissionMode::Default,
            "project yolo must be ignored"
        );
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|w| w.contains("default_mode")),
            "{:?}",
            loaded.report.warnings
        );
    }

    #[test]
    fn layered_load_respects_precedence() {
        let global_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let global = write_config(
            global_dir.path(),
            "[provider]\nmodel = \"global-model\"\nbase_url = \"https://global.example\"\n[cost]\ncurrency = \"usd\"\n",
        );
        let project = write_config(
            project_dir.path(),
            "[provider]\nmodel = \"project-model\"\n",
        );

        // global < project for model; env wins over both.
        let env = |name: &str| (name == MODEL_ENV).then(|| "env-model".to_string());
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
        assert_eq!(
            loaded.config.api_key, None,
            "project api_key must be ignored"
        );
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
    fn project_layer_rejects_runtime_behavior_knobs() {
        let project_dir = tempfile::tempdir().unwrap();
        let project = write_config(
            project_dir.path(),
            "[cost]\nauto_cost_saving = false\n[context]\ncompaction_threshold = 1\n[stream]\nmax_retries = 0\nchunk_timeout_secs = 1\ntotal_timeout_secs = 1\nmax_bytes = 1\n[checkpoints]\nmax_snapshots = 999\n",
        );

        let loaded = AgentConfig::load_with(None, Some(project), &no_env);
        let builtin = AgentConfig::builtin();
        assert_eq!(loaded.config.auto_cost_saving, builtin.auto_cost_saving);
        assert_eq!(
            loaded.config.compaction_threshold,
            builtin.compaction_threshold
        );
        assert_eq!(loaded.config.stream_max_retries, builtin.stream_max_retries);
        assert_eq!(
            loaded.config.stream_chunk_timeout,
            builtin.stream_chunk_timeout
        );
        assert_eq!(
            loaded.config.stream_total_timeout,
            builtin.stream_total_timeout
        );
        assert_eq!(loaded.config.stream_max_bytes, builtin.stream_max_bytes);
        assert_eq!(
            loaded.config.checkpoint_max_snapshots,
            builtin.checkpoint_max_snapshots
        );
        for field in [
            "cost.auto_cost_saving",
            "context.compaction_threshold",
            "stream.max_retries",
            "stream.chunk_timeout_secs",
            "stream.total_timeout_secs",
            "stream.max_bytes",
            "checkpoints.max_snapshots",
        ] {
            assert!(
                loaded
                    .report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(field)),
                "missing rejection warning for {field}"
            );
        }
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
        assert_eq!(loaded.config.stream_chunk_timeout, Duration::from_secs(30));
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
        // layer.error now carries the raw (English) parser error for doctor;
        // the localized wrapper lives in the rendered warning.
        assert!(
            layer
                .error
                .as_deref()
                .is_some_and(|error| !error.is_empty())
        );
        // With no env, `auto` resolves to English, so the wrapper renders in en.
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped"))
        );
    }

    #[test]
    fn config_warnings_render_in_configured_language() {
        let dir = tempfile::tempdir().unwrap();
        // A project file that trips the api_key-ignored warning, plus ui.language.
        let make = |lang: &str| {
            let path = dir.path().join(format!("{lang}.toml"));
            fs::write(
                &path,
                format!("[provider]\napi_key = \"sk-injected\"\n[ui]\nlanguage = \"{lang}\"\n"),
            )
            .unwrap();
            path
        };
        let zh = AgentConfig::load_with(None, Some(make("zh")), &no_env);
        assert!(
            zh.report.warnings.iter().any(|w| w.contains("已忽略")),
            "{:?}",
            zh.report.warnings
        );
        let en = AgentConfig::load_with(None, Some(make("en")), &no_env);
        assert!(
            en.report.warnings.iter().any(|w| w.contains("ignored")),
            "{:?}",
            en.report.warnings
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

    #[test]
    fn approval_auto_allow_only_from_global_or_env() {
        // Project layer must not be able to disarm approval gates.
        let project_dir = tempfile::tempdir().unwrap();
        let project = write_config(
            project_dir.path(),
            "[approval]\nauto_allow = [\"write_\"]\n",
        );
        let loaded = AgentConfig::load_with(None, Some(project), &no_env);
        assert!(loaded.config.approval_auto_allow.is_empty());
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("auto_allow"))
        );

        // Global layer applies.
        let global_dir = tempfile::tempdir().unwrap();
        let global = write_config(
            global_dir.path(),
            "[approval]\nauto_allow = [\"read_\", \" grep_ \", \"\"]\n",
        );
        let loaded = AgentConfig::load_with(Some(global), None, &no_env);
        assert_eq!(
            loaded.config.approval_auto_allow,
            vec!["read_".to_string(), "grep_".to_string()]
        );

        // Env wins and parses comma-separated prefixes.
        let env =
            |name: &str| (name == APPROVAL_AUTO_ALLOW_ENV).then(|| "mock_, git_ ,,".to_string());
        let loaded = AgentConfig::load_with(None, None, &env);
        assert_eq!(
            loaded.config.approval_auto_allow,
            vec!["mock_".to_string(), "git_".to_string()]
        );
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
