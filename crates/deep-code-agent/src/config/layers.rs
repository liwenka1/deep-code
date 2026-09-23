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

/// Parse a setting whose value is one of a fixed set of spellings, warning
/// when the spelling is not one of them instead of silently leaving the field
/// at whatever the layers below it left there.
///
/// One helper rather than a warning written out per setting, because "written
/// out per setting" is exactly how the gap appeared: of the five enum-shaped
/// settings, two had a warning (`provider.reasoning_effort`, `cost.currency`)
/// and three did not (`sandbox.network`, `approval.default_mode`,
/// `ui.language`) — and the silent one that mattered degrades in the
/// PERMISSIVE direction. `[sandbox] network` is the only switch that hard-
/// disables egress, and `NetworkMode::parse` answering `None` drops it back to
/// the builtin `Prompt`: a user who typed `"nevr"` had egress re-armed with
/// nothing said, and under `yolo` (or headless `-p`, where nobody answers a
/// prompt) `yolo_ambient_network` then hands every sandboxed command ambient
/// network. A setting added later cannot repeat it without opting out of this
/// function, and `every_enum_setting_warns_on_an_unrecognized_spelling`
/// enumerates them.
///
/// `field` is the dotted name as the file spells it, so the message names the
/// line the user has to go fix.
fn parse_setting<T>(
    raw: Option<&str>,
    field: &'static str,
    layer: ConfigLayer,
    pending: &mut Vec<PendingWarning>,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let value = raw?.trim();
    // An empty value is "not set", which every caller already treated as such
    // — warning about it would fire on a key the user is in the middle of
    // filling in.
    if value.is_empty() {
        return None;
    }
    let parsed = parse(value);
    if parsed.is_none() {
        pending.push((
            TextId::CfgUnknownValue,
            vec![
                ("layer", layer.label().to_string()),
                ("field", field.to_string()),
                ("value", value.to_string()),
            ],
        ));
    }
    parsed
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
    /// The warnings rendered in the language the assembled config selected —
    /// what the TUI shows.
    pub warnings: Vec<String>,
    pub sources: ConfigSources,
    /// The same warnings before localization, so [`Self::warnings_in`] can
    /// render them in a language other than the config's.
    #[serde(skip)]
    pending: Vec<PendingWarning>,
}

impl ConfigLoadReport {
    /// The load warnings rendered in `lang`, for a surface whose language is
    /// its own rather than the assembled config's.
    ///
    /// The non-TUI surfaces (`doctor`, `serve`, headless `-p`) print English
    /// unconditionally and make no `tr` call of their own — yet they all
    /// printed [`Self::warnings`], which follows `ui.language`. A
    /// `ui.language = "zh"` user therefore got an English report with Chinese
    /// warning rows inside it: the same "one localized fragment per row" shape
    /// that `session_cli`'s age column was fixed for, reached through the
    /// config layer instead of through a `tr` call anyone could grep for.
    /// Rendering on demand from one captured warning is what keeps the two
    /// spellings from drifting.
    #[must_use]
    pub fn warnings_in(&self, lang: Lang) -> Vec<String> {
        self.pending
            .iter()
            .map(|warning| render_warning(lang, warning))
            .collect()
    }
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
                    // Before the overlay, so a file that is one big typo says
                    // so first rather than after the settings it failed to set.
                    for key in file.unknown_keys() {
                        pending.push((
                            TextId::CfgUnknownKey,
                            vec![("layer", layer.label().to_string()), ("field", key)],
                        ));
                    }
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

        apply_env_overlay(&mut config, &mut report.sources, &mut pending, env_lookup);

        // Render deferred warnings now that the final language is known.
        // Resolve through the same `env_lookup` seam so tests stay deterministic.
        let lang = Lang::resolve(&config.language, env_lookup);
        report.warnings = pending
            .iter()
            .map(|warning| render_warning(lang, warning))
            .collect();
        report.pending = pending;
        LoadedAgentConfig { config, report }
    }
}

/// Keys a config file carried that no field of this schema claims.
///
/// `#[serde(flatten)]` into a map is what collects them, and collecting is the
/// point: `#[serde(deny_unknown_fields)]` — which this crate puts on all
/// nineteen model-facing tool-parameter structs — fails the parse, and a
/// failed parse here drops the WHOLE layer through `FileRead::Error`. One
/// misspelled key would then silently disable every correctly-spelled setting
/// beside it, which is a bigger version of the bug this exists to catch.
/// Collecting keeps every valid setting working AND names the typo; it also
/// reports every unknown key at once, where a strict parse stops at the first.
///
/// Why it is needed at all: nothing anywhere read an unknown key. A file with
/// `netwrok = "never"`, `modle = "..."` and a whole invented section loaded
/// with `present: true` and not one warning, so a user who believed they had
/// turned egress off had not.
type UnknownKeys = std::collections::BTreeMap<String, toml::Value>;

/// The section names [`ConfigFile::unknown_keys`] prefixes its per-section
/// findings with, paired with the accessor for that section's leftovers.
///
/// A hand-written list is a drift risk, so it is held to the user-facing
/// schema rather than to itself: `every_documented_section_reports_its_unknown_keys`
/// reads the `[section]` headers out of `config.example.toml`, plants a bogus
/// key under each, and fails naming any section whose typos go unreported.
macro_rules! sections {
    ($file:expr) => {
        [
            ("provider", &$file.provider.unknown),
            ("cost", &$file.cost.unknown),
            ("context", &$file.context.unknown),
            ("stream", &$file.stream.unknown),
            ("approval", &$file.approval.unknown),
            ("checkpoints", &$file.checkpoints.unknown),
            ("ui", &$file.ui.unknown),
            ("lsp", &$file.lsp.unknown),
            ("sandbox", &$file.sandbox.unknown),
        ]
    };
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
    /// Top-level keys and whole sections no field claims.
    #[serde(flatten)]
    unknown: UnknownKeys,
}

impl ConfigFile {
    /// Every key in the file that no field of the schema claims, as the dotted
    /// path the user would go and fix. Sorted, so the warnings come out in a
    /// stable order rather than in `BTreeMap`-per-section order.
    fn unknown_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unknown.keys().cloned().collect();
        for (section, unknown) in sections!(self) {
            out.extend(unknown.keys().map(|key| format!("{section}.{key}")));
        }
        out.sort();
        out
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProviderSection {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    timeout_secs: Option<u64>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CostSection {
    currency: Option<String>,
    auto_cost_saving: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContextSection {
    compaction_threshold: Option<u32>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StreamSection {
    max_retries: Option<u32>,
    chunk_timeout_secs: Option<u64>,
    total_timeout_secs: Option<u64>,
    max_bytes: Option<u64>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ApprovalSection {
    auto_allow: Option<Vec<String>>,
    default_mode: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CheckpointsSection {
    max_snapshots: Option<usize>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UiSection {
    language: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LspSection {
    enabled: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SandboxSection {
    /// `prompt` | `always` | `never`; see [`NetworkMode`].
    network: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
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

    if let Some(effort) = parse_setting(
        file.provider.reasoning_effort.as_deref(),
        "provider.reasoning_effort",
        layer,
        pending,
        ReasoningEffortSetting::parse,
    ) {
        config.reasoning_effort = effort;
        report.sources.reasoning_effort = layer;
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

    if let Some(currency) = parse_setting(
        file.cost.currency.as_deref(),
        "cost.currency",
        layer,
        pending,
        CostCurrency::parse,
    ) {
        config.cost_currency = currency;
        report.sources.cost_currency = layer;
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
    // Validated against the same rule `Lang::resolve` applies later, so an
    // unusable spelling is named here rather than silently falling through to
    // locale detection at render time. `auto` is a real value there, so it has
    // to be accepted here too.
    if let Some(language) = parse_setting(
        file.ui.language.as_deref(),
        "ui.language",
        layer,
        pending,
        |value| {
            (value.eq_ignore_ascii_case("auto") || Lang::from_tag(value).is_some())
                .then(|| value.to_string())
        },
    ) {
        config.language = language;
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
    // egress permissiveness (`always` → `prompt` → `never`) but never raise it.
    // Rejecting only the top rung `always` left a hole — a globally-set `never`
    // was widened back to `prompt` by a hostile checkout, re-arming the
    // approval-gated egress the user turned off. Compare against the *current*
    // value by rank, the same shape the LSP and permission-tier guards use.
    // An unrecognized spelling is warned about by `parse_setting` and leaves
    // the value the layers below set — it does NOT fall back to the builtin.
    if let Some(mode) = parse_setting(
        file.sandbox.network.as_deref(),
        "sandbox.network",
        layer,
        pending,
        NetworkMode::parse,
    ) {
        if project && mode.rank() > config.sandbox_network.rank() {
            pending.push((
                TextId::CfgProjectFieldIgnored,
                vec![("field", format!("sandbox.network={}", mode.as_setting()))],
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

    if let Some(mode) = parse_setting(
        file.approval.default_mode.as_deref(),
        "approval.default_mode",
        layer,
        pending,
        PermissionMode::parse,
    ) {
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

/// Booleans as every other parser in this crate reads them: case-insensitively,
/// with both directions spelled out and anything else `None`.
///
/// `matches!(value.trim(), "1" | "true" | "yes" | "on")` was the old rule, and
/// it failed in two directions at once. `DEEP_CODE_AUTO_COST_SAVING=TRUE` — the
/// spelling half of CI writes — silently meant *false*, and so did a typo, so
/// the only way to learn the setting had not applied was to notice the bill.
/// Returning `None` for an unrecognized word lets [`parse_setting`] say so and
/// leaves the value the layers below set, instead of forcing it off.
fn parse_bool_setting(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The environment layer, applied last and therefore strongest.
///
/// Every setting whose value has a grammar goes through [`parse_setting`], for
/// the reason that function documents: a value the loader cannot read is a
/// setting the user believes they set. The file layer grew that rule and this
/// one did not, so the same typo was reported in
/// `<workspace>/.deep-code/config.toml` and swallowed in the environment —
/// where it is *harder* to notice, because there is no file to re-read.
/// `every_enum_setting_warns_on_an_unrecognized_spelling` covers the file
/// layer; `every_parsed_env_setting_warns_on_an_unrecognized_value` covers this
/// one, and both are enumerations rather than spot checks.
///
/// The free-form settings stay free-form on purpose: an API key, a model id (a
/// model newer than this binary must still be settable — see
/// `ResolutionKind::Passthrough`) and the `auto_allow` tool-name list have no
/// grammar to check against, so there is nothing an unrecognized value could
/// mean.
pub(super) fn apply_env_overlay(
    config: &mut AgentConfig,
    sources: &mut ConfigSources,
    pending: &mut Vec<PendingWarning>,
    lookup: &dyn Fn(&str) -> Option<String>,
) {
    let env = |name: &'static str| lookup(name);
    if let Some(key) = env(DEEPSEEK_API_KEY_ENV).filter(|value| !value.trim().is_empty()) {
        config.api_key = Some(key);
        sources.api_key = ConfigLayer::Env;
    }
    if let Some(model) = env(MODEL_ENV).filter(|value| !value.trim().is_empty()) {
        config.model = model;
        sources.model = ConfigLayer::Env;
    }
    if let Some(effort) = parse_setting(
        env(REASONING_EFFORT_ENV).as_deref(),
        REASONING_EFFORT_ENV,
        ConfigLayer::Env,
        pending,
        ReasoningEffortSetting::parse,
    ) {
        config.reasoning_effort = effort;
        sources.reasoning_effort = ConfigLayer::Env;
    }
    // Validated against the same rule the `ui.language` file key uses, so the
    // two spellings of "is this a language I can render" cannot disagree.
    if let Some(language) = parse_setting(
        env(LANG_ENV).as_deref(),
        LANG_ENV,
        ConfigLayer::Env,
        pending,
        |value| {
            (value.eq_ignore_ascii_case("auto") || Lang::from_tag(value).is_some())
                .then(|| value.to_string())
        },
    ) {
        config.language = language;
    }
    if let Some(value) = parse_setting(
        env(AUTO_COST_SAVING_ENV).as_deref(),
        AUTO_COST_SAVING_ENV,
        ConfigLayer::Env,
        pending,
        parse_bool_setting,
    ) {
        config.auto_cost_saving = value;
    }
    if let Some(currency) = parse_setting(
        env(COST_CURRENCY_ENV).as_deref(),
        COST_CURRENCY_ENV,
        ConfigLayer::Env,
        pending,
        CostCurrency::parse,
    ) {
        config.cost_currency = currency;
        sources.cost_currency = ConfigLayer::Env;
    }
    if let Some(value) = parse_setting(
        env(COMPACTION_THRESHOLD_ENV).as_deref(),
        COMPACTION_THRESHOLD_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<u32>().ok(),
    ) {
        config.compaction_threshold = Some(value);
    }
    if let Some(value) = parse_setting(
        env(STREAM_MAX_RETRIES_ENV).as_deref(),
        STREAM_MAX_RETRIES_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<u32>().ok(),
    ) {
        config.stream_max_retries = value;
    }
    if let Some(value) = parse_setting(
        env(STREAM_CHUNK_TIMEOUT_ENV).as_deref(),
        STREAM_CHUNK_TIMEOUT_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<u64>().ok(),
    ) {
        config.stream_chunk_timeout = Duration::from_secs(value);
    }
    if let Some(value) = parse_setting(
        env(STREAM_TOTAL_TIMEOUT_ENV).as_deref(),
        STREAM_TOTAL_TIMEOUT_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<u64>().ok(),
    ) {
        config.stream_total_timeout = Duration::from_secs(value);
    }
    if let Some(value) = parse_setting(
        env(STREAM_MAX_BYTES_ENV).as_deref(),
        STREAM_MAX_BYTES_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<u64>().ok(),
    ) {
        config.stream_max_bytes = value;
    }
    if let Some(value) = parse_setting(
        env(CHECKPOINT_MAX_SNAPSHOTS_ENV).as_deref(),
        CHECKPOINT_MAX_SNAPSHOTS_ENV,
        ConfigLayer::Env,
        pending,
        |value| value.parse::<usize>().ok(),
    ) {
        config.checkpoint_max_snapshots = value;
    }
    if let Some(value) = env(APPROVAL_AUTO_ALLOW_ENV) {
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
