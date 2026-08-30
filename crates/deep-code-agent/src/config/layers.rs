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
use crate::execution_policy::{NetworkMode, PermissionMode};
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
    sandbox: SandboxSection,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SandboxSection {
    /// `prompt` | `always` | `never`; see [`NetworkMode`].
    network: Option<String>,
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
        // Symmetric with api_key: a project file must not redirect where your
        // credentials + full context go. A malicious repo dropping
        // `base_url = "https://evil"` would otherwise exfiltrate the
        // env/global-config API key on the first turn. Self-hosted endpoints
        // still work — set base_url in the environment or global config.
        if project {
            pending.push((
                TextId::CfgProjectBaseUrlIgnored,
                vec![("url", base_url.to_string())],
            ));
        } else {
            config.base_url = base_url.to_string();
            report.sources.base_url = layer;
        }
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

    // Diagnostics preference, tighten-only from the project layer. Turning LSP
    // *off* is harmless from anywhere, but turning it back *on* is not what the
    // old comment claimed ("a repo can only reduce what runs"): the assignment
    // was unconditional, so a repo that sets `lsp.enabled = true` overrode a
    // user who had globally disabled it — and the server is then spawned with no
    // policy, no approval and no sandbox, while rust-analyzer builds that repo's
    // build scripts and proc macros by default.
    if let Some(enabled) = file.lsp.enabled {
        if project && enabled && !config.lsp_enabled {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![("field", "lsp.enabled=true".to_string())],
            ));
        } else {
            config.lsp_enabled = enabled;
        }
    }

    // Network mode is tighten-only from the project layer: a repo may reduce
    // (`prompt` → `never`) but must not re-arm ambient egress (`always`) —
    // same reasoning as auto/yolo above. Unknown values degrade to unset.
    if let Some(mode) = file.sandbox.network.as_deref().and_then(NetworkMode::parse) {
        if project && mode == NetworkMode::Always {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![("field", "sandbox.network=always".to_string())],
            ));
        } else {
            config.sandbox_network = mode;
        }
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
        // Tighten-only from the project layer: a repo may lower the tier but
        // never raise it. Rejecting only auto/yolo was not enough — a hostile
        // checkout could still raise Default → AcceptEdits and thereby
        // auto-approve every `write_file`/`apply_patch` plus in-workspace
        // `rm/mv/cp/mkdir/touch` from turn one, with no trust-this-folder prompt
        // anywhere. (`config.example.toml` also claimed `approval.*` was ignored
        // in the project layer, so the code was looser than its own docs.)
        // Unknown values degrade to the default.
        if project && mode.to_u8() > config.default_permission_mode.to_u8() {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![(
                    "field",
                    format!("approval.default_mode={}", mode.as_setting()),
                )],
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
mod tests;
