//! Resume compatibility for known rollout inputs. This is deliberately not a
//! fingerprint of the whole host: credentials, process environment and local
//! toolchains are not copied into an evaluation checkpoint.

use std::collections::BTreeSet;

use deep_code_agent::{AgentConfig, Lang, ProjectInstructions, SkillRegistry};
use serde_json::{Value, json};

use crate::bench::BenchmarkInstance;
use crate::runner::EvalConfig;

/// Describe the selected dataset and effective settings without persisting the
/// API key. Concurrency, output location and display format may change on resume.
pub(crate) fn run_signature(
    config: &EvalConfig,
    instances: &[impl BenchmarkInstance],
) -> anyhow::Result<Value> {
    signature_with_environment(config, instances, &EnvironmentSnapshot::capture(config)?)
}

/// Capture only capability switches and prompt inputs read outside AgentConfig.
/// An explicit value also makes compatibility tests independent of process-wide
/// environment mutations and of the developer's personal instruction files.
#[derive(Clone)]
struct EnvironmentSnapshot {
    tools_enabled: bool,
    web_enabled: bool,
    typescript_lsp_trusted: bool,
    effective_language: String,
    global_prompt_sha256: String,
}

impl EnvironmentSnapshot {
    fn capture(config: &EvalConfig) -> anyhow::Result<Self> {
        // An empty workspace lets the existing bounded, policy-aware loader
        // read only global instructions. The temporary path is never included
        // in the fingerprint, so two launches can resume one another.
        let workspace = tempfile::tempdir()?;
        let instructions = ProjectInstructions::load(workspace.path());
        let skills = deep_code_agent::default_config_path()
            .and_then(|path| path.parent().map(|parent| parent.join("skills")))
            .map(|path| SkillRegistry::discover(&path))
            .unwrap_or_default();
        let skill_inputs: Vec<_> = skills
            .list()
            .iter()
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "body": skill.body,
                    // The global path is rendered into the actual skills
                    // index. Hash it with the content, never store it raw.
                    "path": skill.path,
                })
            })
            .collect();
        let global_prompt = json!({
            "instructions": instructions.block(),
            "compact_instructions": instructions.compact_instructions,
            "instruction_warnings": instructions.warnings,
            "skills": skill_inputs,
            "skill_warnings": skills.warnings(),
        });

        Ok(Self {
            tools_enabled: deep_code_agent::tools_enabled(),
            web_enabled: deep_code_agent::web_enabled(),
            typescript_lsp_trusted: typescript_trust(
                std::env::var("DEEP_CODE_TRUST_TYPESCRIPT_LSP")
                    .ok()
                    .as_deref(),
            ),
            effective_language: Lang::from_env(&config.agent_config.language)
                .as_setting()
                .to_string(),
            global_prompt_sha256: sha256(&serde_json::to_vec(&global_prompt)?),
        })
    }
}

fn typescript_trust(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on"
        )
    })
}

fn signature_with_environment(
    config: &EvalConfig,
    instances: &[impl BenchmarkInstance],
    environment: &EnvironmentSnapshot,
) -> anyhow::Result<Value> {
    let mut ids = BTreeSet::new();
    let mut selected = Vec::with_capacity(instances.len());
    for instance in instances {
        let id = instance.instance_id();
        anyhow::ensure!(
            !id.trim().is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']),
            "invalid evaluation instance id: expected a non-empty, single path component"
        );
        anyhow::ensure!(ids.insert(id), "duplicate evaluation instance id: {id}");
        selected.push((
            id,
            json!({
                "instance_id": id,
                "repo": instance.repo(),
                "base_commit": instance.base_commit(),
                "problem_sha256": sha256(instance.problem_statement().as_bytes()),
            }),
        ));
    }
    selected.sort_by_key(|(left, _)| *left);
    let selected: Vec<_> = selected.into_iter().map(|(_, value)| value).collect();

    // Exhaustive on purpose: a new runtime setting must make an explicit
    // compatibility decision here rather than silently mixing two rollouts.
    let AgentConfig {
        api_key: _,
        base_url,
        model,
        reasoning_effort,
        auto_cost_saving,
        cost_currency,
        compaction_threshold,
        timeout,
        stream_max_retries,
        stream_chunk_timeout,
        stream_total_timeout,
        stream_max_bytes,
        approval_auto_allow,
        checkpoint_max_snapshots,
        language,
        notifications: _,
        default_permission_mode,
        lsp_enabled,
        vision_detail,
        sandbox_network,
        sandbox_mode,
        sandbox_allow_irreversible,
        turn_steps,
    } = &config.agent_config;

    Ok(json!({
        "signature_version": 1,
        "agent_version": env!("CARGO_PKG_VERSION"),
        "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "benchmark": {
            "name": config.bench,
            "subset": config.subset,
            "split": config.split,
            "sample": config.sample,
            "instances": selected,
        },
        "instance_timeout": config.instance_timeout,
        "agent": {
            // User-configured endpoints can themselves contain credentials.
            "base_url_sha256": sha256(base_url.as_bytes()),
            "model": model,
            "reasoning_effort": reasoning_effort,
            "auto_cost_saving": auto_cost_saving,
            "cost_currency": cost_currency,
            "compaction_threshold": compaction_threshold,
            "timeout": timeout,
            "stream_max_retries": stream_max_retries,
            "stream_chunk_timeout": stream_chunk_timeout,
            "stream_total_timeout": stream_total_timeout,
            "stream_max_bytes": stream_max_bytes,
            "approval_auto_allow": approval_auto_allow,
            "checkpoint_max_snapshots": checkpoint_max_snapshots,
            "language": language,
            "default_permission_mode": default_permission_mode.as_setting(),
            "lsp_enabled": lsp_enabled,
            "vision_detail": vision_detail.as_str(),
            "sandbox_network": sandbox_network.as_setting(),
            "sandbox_mode": sandbox_mode.as_setting(),
            "sandbox_allow_irreversible": sandbox_allow_irreversible,
            "turn_steps": turn_steps,
        },
        "capabilities": {
            "tools_enabled": environment.tools_enabled,
            "web_enabled": environment.web_enabled,
            "typescript_lsp_trusted": environment.typescript_lsp_trusted,
            "effective_language": environment.effective_language,
            "global_prompt_sha256": environment.global_prompt_sha256,
        },
    }))
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::bench::SweBenchInstance;

    fn config() -> EvalConfig {
        EvalConfig {
            agent_config: AgentConfig::builtin(),
            ..EvalConfig::default()
        }
    }

    fn environment() -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            tools_enabled: true,
            web_enabled: true,
            typescript_lsp_trusted: false,
            effective_language: "en".into(),
            global_prompt_sha256: sha256(b"global prompt"),
        }
    }

    fn instances() -> Vec<SweBenchInstance> {
        ["x__x-1", "x__x-2"]
            .into_iter()
            .map(|id| SweBenchInstance {
                instance_id: id.into(),
                repo: "x/x".into(),
                base_commit: "abc123".into(),
                problem_statement: "Fix the regression".into(),
            })
            .collect()
    }

    #[test]
    fn credentials_and_scheduling_do_not_change_the_signature_or_leak_secrets() {
        let mut first = config();
        first.agent_config.api_key = Some("private-api-key-first".into());
        first.agent_config.base_url = "https://user:private-url-password@example.invalid/v1".into();
        let mut second = first.clone();
        second.agent_config.api_key = Some("private-api-key-second".into());
        second.parallelism = 8;
        second.output_dir = Some("another-output".into());
        second.transcripts_dir = Some("another-transcripts".into());
        second.resume = true;
        second.agent_config.notifications = !second.agent_config.notifications;
        let signature = signature_with_environment(&first, &instances(), &environment()).unwrap();
        assert_eq!(
            signature,
            signature_with_environment(&second, &instances(), &environment()).unwrap()
        );
        let saved = signature.to_string();
        for secret in [
            "private-api-key-first",
            "private-api-key-second",
            "private-url-password",
        ] {
            assert!(
                !saved.contains(secret),
                "credentials must never enter a signature"
            );
        }
    }

    #[test]
    fn rollout_settings_and_problem_content_change_the_signature() {
        let initial = config();
        let env = environment();
        let data = instances();
        let baseline = signature_with_environment(&initial, &data, &env).unwrap();
        let changes: [fn(&mut EvalConfig); 8] = [
            |config| config.agent_config.model = "auto".into(),
            |config| {
                config.agent_config.reasoning_effort =
                    serde_json::from_value(json!("none")).unwrap();
            },
            |config| config.instance_timeout += Duration::from_secs(1),
            |config| config.agent_config.turn_steps += 1,
            |config| config.agent_config.timeout = None,
            |config| config.agent_config.stream_max_retries += 1,
            |config| config.agent_config.lsp_enabled = false,
            |config| config.agent_config.sandbox_network = deep_code_agent::NetworkMode::Never,
        ];
        for change in changes {
            let mut changed = initial.clone();
            change(&mut changed);
            assert_ne!(
                baseline,
                signature_with_environment(&changed, &data, &env).unwrap()
            );
        }
        let mut changed = data;
        changed[0].problem_statement.push_str(" New detail");
        assert_ne!(
            baseline,
            signature_with_environment(&initial, &changed, &env).unwrap()
        );
    }

    #[test]
    fn instance_order_is_irrelevant_but_duplicate_and_unsafe_ids_are_rejected() {
        let config = config();
        let env = environment();
        let mut data = instances();
        let original = signature_with_environment(&config, &data, &env).unwrap();
        data.reverse();
        assert_eq!(
            original,
            signature_with_environment(&config, &data, &env).unwrap()
        );
        data[1] = data[0].clone();
        assert!(signature_with_environment(&config, &data, &env).is_err());
        for id in ["", " ", ".", "..", "../outside", "x/y", "x\\y", "x\0y"] {
            data.truncate(1);
            data[0].instance_id = id.into();
            assert!(
                signature_with_environment(&config, &data, &env).is_err(),
                "{id:?} is not a safe instance id"
            );
        }
    }

    #[test]
    fn effective_capabilities_and_global_prompt_are_resume_inputs() {
        let config = config();
        let data = instances();
        let env = environment();
        let baseline = signature_with_environment(&config, &data, &env).unwrap();
        let changes: [fn(&mut EnvironmentSnapshot); 5] = [
            |env| env.tools_enabled = false,
            |env| env.web_enabled = false,
            |env| env.typescript_lsp_trusted = true,
            |env| env.effective_language = "zh".into(),
            |env| env.global_prompt_sha256 = sha256(b"edited global prompt"),
        ];
        for change in changes {
            let mut changed = env.clone();
            change(&mut changed);
            assert_ne!(
                baseline,
                signature_with_environment(&config, &data, &changed).unwrap()
            );
        }
    }

    #[test]
    fn typescript_trust_uses_the_managers_effective_boolean_rules() {
        for value in [None, Some("false"), Some("yes"), Some(""), Some("tru")] {
            assert!(!typescript_trust(value));
        }
        for value in ["1", "true", " ON ", "TrUe"] {
            assert!(typescript_trust(Some(value)));
        }
    }

    #[test]
    fn repeated_capture_does_not_fingerprint_the_temporary_workspace() {
        let config = config();
        let data = instances();
        assert_eq!(
            run_signature(&config, &data).unwrap(),
            run_signature(&config, &data).unwrap()
        );
    }
}
