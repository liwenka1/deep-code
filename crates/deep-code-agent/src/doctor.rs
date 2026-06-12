//! Machine-readable health and capability report for local supervisors.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{AgentConfig, ConfigLoadReport, DEEPSEEK_API_KEY_ENV};
use crate::error::api_key_setup_hint;
use crate::hooks::default_hooks_config_path;
use crate::mcp::{McpManager, McpServerStatus, default_mcp_config_path, workspace_mcp_config_path};
use crate::model_registry::ModelRegistry;
use crate::sandbox::detect_capabilities;
use crate::skills::{discover_in_workspace, global_skills_dir, workspace_skills_dir};

/// Path of the global user config file loaded by [`AgentConfig::load`].
#[must_use]
pub fn default_config_path() -> PathBuf {
    home_dir()
        .map(|home| home.join(".deep-code").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from(".deep-code/config.toml"))
}

/// How the layered configuration was assembled, for `deep-code doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigLayersDoctorReport {
    pub layers: Vec<ConfigLayerDoctorEntry>,
    pub model_source: String,
    pub base_url_source: String,
    pub currency_source: String,
    pub api_key_source: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigLayerDoctorEntry {
    pub name: String,
    pub path: String,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<&ConfigLoadReport> for ConfigLayersDoctorReport {
    fn from(report: &ConfigLoadReport) -> Self {
        Self {
            layers: report
                .layers
                .iter()
                .map(|layer| ConfigLayerDoctorEntry {
                    name: layer.name.to_string(),
                    path: layer.path.clone(),
                    present: layer.present,
                    error: layer.error.clone(),
                })
                .collect(),
            model_source: report.sources.model.label().to_string(),
            base_url_source: report.sources.base_url.label().to_string(),
            currency_source: report.sources.cost_currency.label().to_string(),
            api_key_source: report.sources.api_key.label().to_string(),
            warnings: report.warnings.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyReport {
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxReport {
    pub available: bool,
    pub kind: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerDoctorEntry {
    pub name: String,
    pub enabled: bool,
    pub status: String,
    pub detail: String,
    pub tool_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpDoctorReport {
    pub config_path: String,
    pub workspace_config_path: String,
    pub present: bool,
    pub servers: Vec<McpServerDoctorEntry>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsDirectoryReport {
    pub path: String,
    pub present: bool,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsDoctorReport {
    pub total_count: usize,
    pub directories: Vec<SkillsDirectoryReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HooksDoctorReport {
    pub config_path: String,
    pub present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelDoctorEntry {
    pub id: String,
    pub context_window: u32,
    pub supports_reasoning: bool,
    pub supports_tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeepSeekDoctorReport {
    pub auto_model: bool,
    pub reasoning_effort: String,
    pub cost_currency: String,
    pub beta_endpoint: bool,
    pub models: Vec<ModelDoctorEntry>,
    pub api_key_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub config_path: String,
    pub config_present: bool,
    pub workspace: String,
    pub api_key: ApiKeyReport,
    pub base_url: String,
    pub default_model: String,
    pub deepseek: DeepSeekDoctorReport,
    pub sandbox: SandboxReport,
    pub mcp: McpDoctorReport,
    pub skills: SkillsDoctorReport,
    pub hooks: HooksDoctorReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_layers: Option<ConfigLayersDoctorReport>,
}

impl DoctorReport {
    #[must_use]
    pub fn collect(workspace: &Path, config: &AgentConfig) -> Self {
        let config_path = default_config_path();
        let config_present = config_path.is_file();
        let sandbox = detect_capabilities();
        let mcp = collect_mcp(workspace);
        let skills = collect_skills(workspace);
        let hooks_path = default_hooks_config_path();
        let deepseek = collect_deepseek(config);

        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            config_path: config_path.display().to_string(),
            config_present,
            workspace: workspace.display().to_string(),
            api_key: api_key_report(config),
            base_url: config.base_url.clone(),
            default_model: config.model.clone(),
            deepseek,
            sandbox: SandboxReport {
                available: sandbox.available,
                kind: if sandbox.available {
                    Some(sandbox.backend.id().to_string())
                } else {
                    None
                },
                detail: sandbox.detail,
            },
            mcp,
            skills,
            hooks: HooksDoctorReport {
                config_path: hooks_path.display().to_string(),
                present: hooks_path.is_file(),
            },
            config_layers: None,
        }
    }

    /// Attach the layered-config assembly report (from [`AgentConfig::load`]).
    ///
    /// Also replaces the env-sniffing api-key source heuristic with the
    /// authoritative layer recorded by the loader (env/global), keeping
    /// "missing" when no key was set anywhere.
    #[must_use]
    pub fn with_config_layers(mut self, report: &ConfigLoadReport) -> Self {
        if report.sources.api_key != crate::config::ConfigLayer::Builtin {
            self.api_key.source = report.sources.api_key.label().to_string();
        }
        self.config_layers = Some(ConfigLayersDoctorReport::from(report));
        self
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

fn collect_deepseek(config: &AgentConfig) -> DeepSeekDoctorReport {
    let registry = ModelRegistry::default();
    let models = registry
        .list()
        .iter()
        .map(|model| ModelDoctorEntry {
            id: model.id.clone(),
            context_window: model.context_window,
            supports_reasoning: model.supports_reasoning,
            supports_tools: model.supports_tools,
        })
        .collect();

    DeepSeekDoctorReport {
        auto_model: config.auto_model_enabled(),
        reasoning_effort: config.reasoning_effort.as_setting().to_string(),
        cost_currency: format!("{:?}", config.cost_currency).to_ascii_lowercase(),
        beta_endpoint: config.uses_beta_endpoint(),
        models,
        api_key_hint: api_key_setup_hint().to_string(),
    }
}

fn api_key_report(config: &AgentConfig) -> ApiKeyReport {
    let source = if config
        .api_key
        .as_ref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        if std::env::var(DEEPSEEK_API_KEY_ENV)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .is_some()
        {
            "env".to_string()
        } else {
            "inline".to_string()
        }
    } else {
        "missing".to_string()
    };
    ApiKeyReport { source }
}

fn collect_mcp(workspace: &Path) -> McpDoctorReport {
    let global_path = default_mcp_config_path();
    let workspace_path = workspace_mcp_config_path(workspace);
    let present = global_path.is_file() || workspace_path.is_file();
    let manager = McpManager::load_from_workspace(workspace).unwrap_or_default();
    let report = manager.validate();
    let servers = report
        .servers
        .into_iter()
        .map(|server| {
            let (status, detail) = match &server.status {
                McpServerStatus::Ready => ("ok".to_string(), "ready".to_string()),
                McpServerStatus::Disabled => ("disabled".to_string(), "disabled".to_string()),
                McpServerStatus::Failed { error } => ("error".to_string(), error.clone()),
            };
            McpServerDoctorEntry {
                name: server.name,
                enabled: server.enabled,
                status,
                detail,
                tool_count: server.tool_count,
            }
        })
        .collect();

    McpDoctorReport {
        config_path: global_path.display().to_string(),
        workspace_config_path: workspace_path.display().to_string(),
        present,
        servers,
        errors: report.errors,
    }
}

fn collect_skills(workspace: &Path) -> SkillsDoctorReport {
    let registry = discover_in_workspace(workspace);
    let dirs = [
        workspace_skills_dir(workspace),
        workspace.join(".deep-code").join("skills"),
        global_skills_dir(),
    ];
    let mut directories = Vec::new();
    for path in dirs {
        let present = path.is_dir();
        let count = if present {
            registry
                .list()
                .iter()
                .filter(|skill| skill.path.starts_with(&path))
                .count()
        } else {
            0
        };
        directories.push(SkillsDirectoryReport {
            path: path.display().to_string(),
            present,
            count,
        });
    }
    SkillsDoctorReport {
        total_count: registry.len(),
        directories,
        warnings: registry.warnings().to_vec(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn doctor_report_serializes_to_json() {
        let workspace = TempDir::new().unwrap();
        let config = AgentConfig::default();
        let report = DoctorReport::collect(workspace.path(), &config);
        let json = report.to_json_pretty().unwrap();
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"sandbox\""));
        assert!(json.contains("\"mcp\""));
    }

    #[test]
    fn api_key_missing_when_unset() {
        let config = AgentConfig {
            api_key: None,
            ..AgentConfig::default()
        };
        assert_eq!(api_key_report(&config).source, "missing");
    }
}
