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
fn sandbox_network_is_tighten_only_from_the_project_layer() {
    use crate::execution_policy::NetworkMode;
    assert_eq!(AgentConfig::builtin().sandbox_network, NetworkMode::Prompt);

    // The user's global file may opt back into ambient egress.
    let global_dir = tempfile::tempdir().unwrap();
    let global = write_config(global_dir.path(), "[sandbox]\nnetwork = \"always\"\n");
    let loaded = AgentConfig::load_with(Some(global.clone()), None, &no_env);
    assert_eq!(loaded.config.sandbox_network, NetworkMode::Always);

    // A project file may tighten (here: over a permissive global)…
    let project_dir = tempfile::tempdir().unwrap();
    let project = write_config(project_dir.path(), "[sandbox]\nnetwork = \"never\"\n");
    let loaded = AgentConfig::load_with(Some(global), Some(project), &no_env);
    assert_eq!(loaded.config.sandbox_network, NetworkMode::Never);

    // …but must not re-arm ambient egress: a repo saying "always" is
    // ignored with a warning, same class as auto/yolo mode injection.
    let project_dir = tempfile::tempdir().unwrap();
    let project = write_config(project_dir.path(), "[sandbox]\nnetwork = \"always\"\n");
    let loaded = AgentConfig::load_with(None, Some(project), &no_env);
    assert_eq!(loaded.config.sandbox_network, NetworkMode::Prompt);
    assert!(
        loaded
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("sandbox.network")),
        "ignoring a repo's always must be surfaced: {:?}",
        loaded.report.warnings
    );

    // …and must not widen a globally-set `never` up to `prompt` either. This is
    // the case the old `== Always` guard missed: `prompt` is more permissive
    // than `never` (it re-arms approval-gated egress), so a repo raising it is a
    // widen just like `always`, and must be ignored with a warning.
    let strict_dir = tempfile::tempdir().unwrap();
    let global = write_config(strict_dir.path(), "[sandbox]\nnetwork = \"never\"\n");
    let project_dir = tempfile::tempdir().unwrap();
    let project = write_config(project_dir.path(), "[sandbox]\nnetwork = \"prompt\"\n");
    let loaded = AgentConfig::load_with(Some(global), Some(project), &no_env);
    assert_eq!(
        loaded.config.sandbox_network,
        NetworkMode::Never,
        "project prompt must not widen a global never"
    );
    assert!(
        loaded
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("sandbox.network")),
        "widening never→prompt must be surfaced: {:?}",
        loaded.report.warnings
    );

    // Unknown values degrade to unset instead of failing the load.
    let global_dir = tempfile::tempdir().unwrap();
    let global = write_config(global_dir.path(), "[sandbox]\nnetwork = \"sometimes\"\n");
    let loaded = AgentConfig::load_with(Some(global), None, &no_env);
    assert_eq!(loaded.config.sandbox_network, NetworkMode::Prompt);
}

/// The project layer is tighten-only for the permission tier. Rejecting just
/// auto/yolo was not enough: raising `default` → `accept_edits` already
/// auto-approves every workspace write plus in-workspace `rm/mv/cp/mkdir`,
/// and a repo is untrusted input parsed before any UI is drawn.
#[test]
fn project_layer_may_only_lower_the_permission_tier() {
    use crate::execution_policy::PermissionMode;
    let dir = tempfile::tempdir().unwrap();

    // Global config may set any mode, including yolo.
    let global = write_config(dir.path(), "[approval]\ndefault_mode = \"yolo\"\n");
    let loaded = AgentConfig::load_with(Some(global), None, &no_env);
    assert_eq!(loaded.config.default_permission_mode, PermissionMode::Yolo);

    // A project file may LOWER the tier the global config chose.
    let strict_dir = tempfile::tempdir().unwrap();
    let strict = write_config(
        strict_dir.path(),
        "[approval]\ndefault_mode = \"default\"\n",
    );
    let global = write_config(dir.path(), "[approval]\ndefault_mode = \"yolo\"\n");
    let loaded = AgentConfig::load_with(Some(global), Some(strict), &no_env);
    assert_eq!(
        loaded.config.default_permission_mode,
        PermissionMode::Default,
        "project must be able to tighten"
    );

    // It may NOT raise it — not even by one notch, and not silently.
    for evil_mode in ["accept_edits", "auto", "yolo"] {
        let evil_dir = tempfile::tempdir().unwrap();
        let evil = write_config(
            evil_dir.path(),
            &format!("[approval]\ndefault_mode = \"{evil_mode}\"\n"),
        );
        let loaded = AgentConfig::load_with(None, Some(evil), &no_env);
        assert_eq!(
            loaded.config.default_permission_mode,
            PermissionMode::Default,
            "project {evil_mode} must be ignored"
        );
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|w| w.contains("default_mode")),
            "{evil_mode}: {:?}",
            loaded.report.warnings
        );
    }
}

/// `lsp.enabled` is tighten-only too: a repo may turn the language server
/// off, but must not turn one back on for a user who disabled it globally —
/// the server is spawned with no policy, no approval and no sandbox, and
/// rust-analyzer builds the repo's build scripts and proc macros by default.
#[test]
fn project_layer_may_disable_lsp_but_not_enable_it() {
    let off_dir = tempfile::tempdir().unwrap();
    let off = write_config(off_dir.path(), "[lsp]\nenabled = false\n");
    let loaded = AgentConfig::load_with(None, Some(off), &no_env);
    assert!(!loaded.config.lsp_enabled, "project may turn LSP off");

    let global_dir = tempfile::tempdir().unwrap();
    let evil_dir = tempfile::tempdir().unwrap();
    let global = write_config(global_dir.path(), "[lsp]\nenabled = false\n");
    let evil = write_config(evil_dir.path(), "[lsp]\nenabled = true\n");
    let loaded = AgentConfig::load_with(Some(global), Some(evil), &no_env);
    assert!(
        !loaded.config.lsp_enabled,
        "project must not re-enable a globally disabled LSP"
    );
    assert!(
        loaded.report.warnings.iter().any(|w| w.contains("lsp")),
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
fn project_layer_rejects_api_key_and_base_url() {
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
    assert_eq!(
        loaded.config.base_url,
        AgentConfig::builtin().base_url,
        "project base_url must be ignored — a repo can't redirect the endpoint"
    );
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
        "[approval]\nauto_allow = [\"write_file\"]\n",
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
        // Gated tools, matching config.example.toml's own advice: it
        // switched its sample away from read_file/list_dir/grep_files
        // because those are never gated, so putting them in auto_allow is
        // a no-op — and a fixture reads as an endorsed config even when
        // it is only exercising the parser.
        "[approval]\nauto_allow = [\"write_file\", \" apply_patch \", \"\"]\n",
    );
    let loaded = AgentConfig::load_with(Some(global), None, &no_env);
    assert_eq!(
        loaded.config.approval_auto_allow,
        vec!["write_file".to_string(), "apply_patch".to_string()]
    );

    // Env wins, and splits on commas with surrounding space trimmed and
    // empty entries dropped. Spelled with real tool names that are actually
    // GATED, matching the global fixture above: `read_file`/`grep_files`
    // are real names but need no approval, so an entry naming them does
    // nothing — which makes them a poor illustration of what the setting is
    // for, and (unlike a `read_`-style prefix) is NOT what launch warns
    // about either, since `warn_unmatched_auto_allow` deliberately stays
    // silent for a matched-but-ungated tool.
    let env = |name: &str| {
        (name == APPROVAL_AUTO_ALLOW_ENV).then(|| "write_file, apply_patch ,,".to_string())
    };
    let loaded = AgentConfig::load_with(None, None, &env);
    assert_eq!(
        loaded.config.approval_auto_allow,
        vec!["write_file".to_string(), "apply_patch".to_string()]
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

/// Every setting whose value is one of a fixed set of spellings must say so
/// when the spelling is not one of them.
///
/// Two of the five had a warning and three did not, and the split was
/// invisible: each site spelled its own `and_then(X::parse)`, so "this one
/// warns" and "this one is silent" looked identical at a glance. The one that
/// mattered degrades PERMISSIVELY — `[sandbox] network` is the only switch
/// that hard-disables egress, and an unrecognized value dropped it back to
/// `prompt`. Enumerated rather than spot-checked so a sixth setting cannot
/// join the silent side.
#[test]
fn every_enum_setting_warns_on_an_unrecognized_spelling() {
    // (section, key, a spelling no parser accepts)
    const SETTINGS: [(&str, &str, &str); 5] = [
        ("provider", "reasoning_effort", "maximum"),
        ("cost", "currency", "eur"),
        ("sandbox", "network", "nevr"),
        ("approval", "default_mode", "yoloo"),
        ("ui", "language", "kling0n"),
    ];

    for (section, key, bad) in SETTINGS {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &format!("[{section}]\n{key} = \"{bad}\"\n"));
        let loaded = AgentConfig::load_with(Some(path), None, &no_env);
        let field = format!("{section}.{key}");
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains(&field) && warning.contains(bad)),
            "{field}='{bad}' must be reported, got {:?}",
            loaded.report.warnings
        );
    }
}

/// The environment twin of the sweep above.
///
/// The file layer grew `parse_setting` and this one did not, so the same typo
/// was named in `config.toml` and swallowed in the environment — the harder
/// place to notice it, since there is no file to re-read. Every env setting
/// with a grammar is listed here; a new one cannot be added silently without
/// this failing or someone deciding to leave it out.
#[test]
fn every_parsed_env_setting_warns_on_an_unrecognized_value() {
    use crate::config::{
        AUTO_COST_SAVING_ENV, CHECKPOINT_MAX_SNAPSHOTS_ENV, COMPACTION_THRESHOLD_ENV,
        COST_CURRENCY_ENV, LANG_ENV, REASONING_EFFORT_ENV, STREAM_CHUNK_TIMEOUT_ENV,
        STREAM_MAX_BYTES_ENV, STREAM_MAX_RETRIES_ENV, STREAM_TOTAL_TIMEOUT_ENV,
    };

    const SETTINGS: [(&str, &str); 10] = [
        (REASONING_EFFORT_ENV, "maximum"),
        (LANG_ENV, "kling0n"),
        (AUTO_COST_SAVING_ENV, "affirmative"),
        (COST_CURRENCY_ENV, "eur"),
        (COMPACTION_THRESHOLD_ENV, "lots"),
        (STREAM_MAX_RETRIES_ENV, "many"),
        (STREAM_CHUNK_TIMEOUT_ENV, "5m"),
        (STREAM_TOTAL_TIMEOUT_ENV, "-1"),
        (STREAM_MAX_BYTES_ENV, "50MB"),
        (CHECKPOINT_MAX_SNAPSHOTS_ENV, "twenty"),
    ];

    for (name, bad) in SETTINGS {
        let loaded = AgentConfig::load_with(None, None, &|asked| {
            (asked == name).then(|| bad.to_string())
        });
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains(name) && warning.contains(bad)),
            "{name}='{bad}' must be reported, got {:?}",
            loaded.report.warnings
        );
    }
}

/// The half of the old boolean rule that failed *toward* the expensive answer:
/// an uppercase `TRUE` meant false, silently. Both spellings now work, and an
/// unrecognized word leaves the value alone instead of forcing it off.
#[test]
fn an_env_boolean_reads_case_insensitively_and_both_ways() {
    use crate::config::AUTO_COST_SAVING_ENV;

    let load = |value: &'static str| {
        AgentConfig::load_with(None, None, &move |asked| {
            (asked == AUTO_COST_SAVING_ENV).then(|| value.to_string())
        })
    };

    for on in ["1", "true", "TRUE", "Yes", "ON"] {
        assert!(load(on).config.auto_cost_saving, "{on} must read as on");
    }
    for off in ["0", "false", "FALSE", "no", "Off"] {
        assert!(!load(off).config.auto_cost_saving, "{off} must read as off");
    }

    // Unrecognized: warned about, and the builtin value is left standing
    // rather than being forced to `false` by the failed parse.
    let loaded = load("affirmative");
    assert_eq!(
        loaded.config.auto_cost_saving,
        AgentConfig::builtin().auto_cost_saving
    );
    assert!(
        loaded
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("affirmative"))
    );
}

/// The regression that motivated the sweep above, spelled out on its own
/// because its consequence is not "a setting did not apply".
///
/// A misspelled `never` leaves `sandbox_network` at the builtin `Prompt`, and
/// `yolo_ambient_network` hands every sandboxed command ambient egress under
/// any mode but `Never`. With nobody at a prompt (`yolo`, headless `-p`) that
/// is egress re-armed with nothing said.
#[test]
fn a_misspelled_network_never_does_not_silently_re_arm_egress() {
    use crate::execution_policy::NetworkMode;

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), "[sandbox]\nnetwork = \"nevr\"\n");
    let loaded = AgentConfig::load_with(Some(path), None, &no_env);

    // The value really does stay permissive — this test is about the WARNING,
    // not about guessing what the user meant.
    assert_eq!(loaded.config.sandbox_network, NetworkMode::Prompt);
    assert!(
        loaded
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("sandbox.network") && warning.contains("nevr")),
        "the one switch that disables egress must not fail silently: {:?}",
        loaded.report.warnings
    );
}

/// A key no field claims is reported, at the top level and inside every
/// documented section.
///
/// The section list `unknown_keys` walks is hand-written, so it is held
/// against the user-facing schema instead of against itself: the sections come
/// out of `config.example.toml`, which is the file the warning tells people to
/// check. A section added to the example (or to the structs) without being
/// wired into `sections!` fails here.
#[test]
fn every_documented_section_reports_its_unknown_keys() {
    let example = include_str!("../../../../../config.example.toml");
    let sections: Vec<&str> = example
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix('[')?.strip_suffix(']'))
        .filter(|name| !name.starts_with('['))
        .collect();
    assert!(
        sections.len() >= 9,
        "config.example.toml should document every section, found {sections:?}"
    );

    let mut file = String::from("bogus_top_level = 1\n");
    for section in &sections {
        file.push_str(&format!("\n[{section}]\nbogus_key_here = 1\n"));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &file);
    let loaded = AgentConfig::load_with(Some(path), None, &no_env);

    assert!(
        loaded
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("bogus_top_level")),
        "an unknown top-level key must be reported: {:?}",
        loaded.report.warnings
    );
    for section in &sections {
        let dotted = format!("{section}.bogus_key_here");
        assert!(
            loaded
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains(&dotted)),
            "{dotted} must be reported: {:?}",
            loaded.report.warnings
        );
    }
}

/// A valid file warns about nothing — the unknown-key sweep must not fire on
/// the schema it is checking against.
///
/// `config.example.toml` is the obvious witness: it is what the warning tells
/// people to compare their file with, so if IT tripped the warning the advice
/// would be self-defeating. Commented-out lines make it load as an empty file,
/// so the keys are uncommented first.
#[test]
fn the_documented_example_config_produces_no_warnings() {
    let example = include_str!("../../../../../config.example.toml");
    let uncommented: String = example
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.strip_prefix("# ") {
                // Only revive lines that look like `key = value`; prose comments
                // stay comments.
                Some(rest) if rest.contains(" = ") => rest.to_string(),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &uncommented);
    let loaded = AgentConfig::load_with(Some(path), None, &no_env);
    // The revived file has to LOAD first, or this test proves nothing: a prose
    // comment carrying a ` = ` is revived too, the file then fails to parse,
    // the only warning is "unusable" rather than "unknown key", and the sweep
    // below passes without ever running. That is exactly what it did — three
    // prose lines in the Chinese original revived into broken TOML — so the
    // parse is asserted before the sweep, not left implied.
    let parse_errors: Vec<&String> = loaded
        .report
        .layers
        .iter()
        .filter_map(|layer| layer.error.as_ref())
        .collect();
    assert!(
        parse_errors.is_empty(),
        "the revived example must still be valid TOML — keep ` = ` out of prose \
         comments: {parse_errors:?}"
    );
    let unexpected: Vec<&String> = loaded
        .report
        .warnings
        .iter()
        .filter(|warning| warning.contains("unknown key"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "config.example.toml must match the schema it documents: {unexpected:?}"
    );
}

/// The non-TUI surfaces print English; `warnings` follows `ui.language`. Both
/// renderings must come from the one captured warning, so they cannot drift.
#[test]
fn warnings_render_in_the_callers_language_not_only_the_configs() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        dir.path(),
        "[ui]\nlanguage = \"zh\"\n[cost]\ncurrency = \"eur\"\n",
    );
    let loaded = AgentConfig::load_with(Some(path), None, &no_env);

    let localized = loaded.report.warnings.join("\n");
    assert!(
        localized.chars().any(|ch| ch as u32 > 0x4e00),
        "the TUI keeps the configured language: {localized:?}"
    );

    let english = loaded.report.warnings_in(Lang::En).join("\n");
    assert!(
        english.is_ascii(),
        "doctor/serve/-p must be able to render the same warning in English: {english:?}"
    );
    assert!(english.contains("cost.currency"));
}
