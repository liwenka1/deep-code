//! `deep-code doctor` command.

use deep_code_agent::{AgentConfig, DoctorReport, Enforcement, SandboxMode};

use crate::cli::workspace_root;

/// One confinement dimension, as the non-JSON report words it. `partial` is its
/// own answer on purpose: collapsing it into `yes` would repeat the claim this
/// report exists to avoid, and into `NO` would understate a boundary that does
/// hold for everything except the named gaps.
fn enforcement_label(enforcement: &Enforcement) -> &'static str {
    match enforcement {
        Enforcement::Full => "yes",
        Enforcement::Partial { .. } => "partial",
        Enforcement::None => "NO",
    }
}

/// The one-word sandbox state the text report leads with.
///
/// `disabled` is its own answer: under `[sandbox] mode = "off"` the host may
/// well have a backend, so `unavailable` would blame the host for a setting —
/// and before `available` folded the switch in, this read `enforcing`'s
/// neighbours and the lines under it told the user their network setting had
/// "no effect on this platform", a platform that enforces it fine.
fn sandbox_state(report: &DoctorReport, overall: &Enforcement) -> &'static str {
    if report.sandbox.mode_setting == SandboxMode::Off.as_setting() {
        return "disabled";
    }
    if !report.sandbox.available {
        return "unavailable";
    }
    match overall {
        Enforcement::Full => "enforcing",
        Enforcement::Partial { .. } => "partial",
        Enforcement::None => "not enforcing",
    }
}

/// Print the report.
///
/// The frame is English throughout — every label here, not just most of them.
/// Three of them used to be Chinese (`错误`, `警告`, `api key 引导`) while the
/// twenty-odd around them were not, so an English-locale host read its own
/// diagnostics half in a language it had not asked for. `doctor` is a
/// diagnostic surface with no `/lang` behind it and no `TextId` anywhere in
/// this file; the one localized thing it prints is
/// `report.deepseek.api_key_hint`, which the agent crate resolves against the
/// configured language because it is user-facing *guidance* rather than a label
/// on a field. Adding a language to the labels would mean localizing all of
/// them; matching the rest is the smaller true statement.
pub fn run_doctor(json: bool) -> anyhow::Result<()> {
    let workspace = workspace_root();
    let loaded = AgentConfig::load(&workspace);
    let report =
        DoctorReport::collect(&workspace, &loaded.config).with_config_layers(&loaded.report);

    if json {
        println!("{}", report.to_json_pretty()?);
        return Ok(());
    }

    let clean = |text: &str| deep_code_agent::neutralize_display_text(text);

    println!("deep-code doctor");
    println!("  version: {}", report.version);
    println!("  workspace: {}", clean(&report.workspace));
    println!(
        "  config: {} (present={})",
        clean(&report.config_path),
        report.config_present
    );
    // Everything below that can carry repo-controlled text goes through the
    // sanitizer: layer paths and the raw `toml::de::Error` (which echoes the
    // offending source line), the layer warnings (which interpolate
    // `provider.base_url` and friends), and `default_model`/`base_url`, which
    // a project config can override. `doctor` is a real terminal like any
    // other; it was simply outside the module that owned the rule.
    if let Some(layers) = &report.config_layers {
        for layer in &layers.layers {
            match &layer.error {
                Some(error) => println!(
                    "    layer {}: {} (present={}, error: {})",
                    layer.name,
                    clean(&layer.path),
                    layer.present,
                    clean(error)
                ),
                None => println!(
                    "    layer {}: {} (present={})",
                    layer.name,
                    clean(&layer.path),
                    layer.present
                ),
            }
        }
        println!(
            "    sources: model={} base_url={} currency={} api_key={}",
            layers.model_source,
            layers.base_url_source,
            layers.currency_source,
            layers.api_key_source
        );
        for warning in &layers.warnings {
            println!("    warning: {}", clean(warning));
        }
    }
    println!("  api key: {}", report.api_key.source);
    println!(
        "  model: {} @ {}",
        clean(&report.default_model),
        clean(&report.base_url)
    );
    println!(
        "  deepseek: auto_model={} reasoning={} currency={} beta={} vision_detail={}",
        report.deepseek.auto_model,
        report.deepseek.reasoning_effort,
        report.deepseek.cost_currency,
        report.deepseek.beta_endpoint,
        report.deepseek.vision_detail
    );
    for model in &report.deepseek.models {
        // What the model CAN do, as names. A row of `vision=false` flags is
        // read by comparing lines, and the comparison that matters — Flash
        // accepts an image, Pro does not — is exactly what a name list makes
        // visible. `fim` is kept separate because its third state is not a flag,
        // and a model with no capabilities prints `-` rather than an empty value
        // that reads like a formatting bug.
        let capabilities = [
            (model.supports_reasoning, "reasoning"),
            (model.supports_json_output, "json-output"),
            (model.supports_tools, "tools"),
            (model.supports_responses_api, "responses-api"),
            (model.supports_anthropic_api, "anthropic-api"),
            (model.supports_prefix_completion, "prefix-completion"),
            (model.supports_vision, "vision"),
        ]
        .into_iter()
        .filter_map(|(present, name)| present.then_some(name))
        .collect::<Vec<&str>>();
        let capabilities = if capabilities.is_empty() {
            "-".to_string()
        } else {
            capabilities.join(",")
        };
        // `version` and `concurrency_limit` are printed because they are the two
        // fields nothing consumes yet, and an unread field is one that rots.
        println!(
            "    - {} v={} ctx={} max_out={} conc={} caps={} fim={}",
            model.id,
            model.version,
            model.context_window,
            model.max_output,
            model.concurrency_limit,
            capabilities,
            model.fim_completion
        );
    }
    // The report decides whether guidance applies (`api_key_hint` is `Some`
    // exactly when no usable key was assembled); this surface no longer
    // re-derives it from `api_key.source`. Two spellings of one predicate is
    // how the JSON surface came to print the "missing key" paragraph on a host
    // that had one.
    if let Some(hint) = &report.deepseek.api_key_hint {
        println!("  api key setup:\n{hint}");
    }
    // "available" is not the same as "enforcing": a backend can exist and still
    // confine nothing (Windows Job Object). Report what it actually does.
    // One definition of "what does this host enforce overall", shared with the
    // approval panel and the tool descriptions. Deriving it here by hand meant
    // two, and they had drifted: a Windows host (a backend that exists and
    // confines nothing) printed `partial`, claiming a boundary with holes where
    // there is no boundary at all — while the two lines below it said `NO`.
    let overall = Enforcement::weakest(
        report.sandbox.filesystem.clone(),
        report.sandbox.network.clone(),
    );
    println!(
        "  sandbox: {} ({})",
        sandbox_state(&report, &overall),
        report.sandbox.detail
    );
    // The configured egress policy, beside what the host can enforce. Without
    // it `[sandbox] network = "never"` was unconfirmable: a typo in the value
    // (or in the key) produced a report identical to one where it had taken
    // effect, on the one setting that hard-disables egress.
    println!("    [sandbox] network = {}", report.sandbox.network_setting);
    // …and the sandbox switch, for the same reason one line up: it is a
    // configuration value that decides what the gate DOES, and `off` removes the
    // confinement every other line of this report describes.
    println!("    [sandbox] mode = {}", report.sandbox.mode_setting);
    if report.sandbox.available && !overall.is_full() {
        println!(
            "    workspace-write confinement: {}",
            enforcement_label(&report.sandbox.filesystem)
        );
        println!(
            "    network withheld by default: {}",
            enforcement_label(&report.sandbox.network)
        );
        for gap in report
            .sandbox
            .filesystem
            .gaps()
            .iter()
            .chain(report.sandbox.network.gaps())
        {
            println!("      ! {}", gap.detail());
        }
        // Only say this where it is true. A Landlock host with an older kernel
        // is partial on writes while seccomp still withholds the network, and
        // the old unconditional line told those users their `network` setting
        // was inert when it was doing its job.
        if !report.sandbox.network.is_enforced() {
            println!("    → [sandbox] network has no effect on this platform.");
        }
    }
    println!("  skills: {} loaded", report.skills.total_count);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mode = "off"` reads as a setting, not as a host without a sandbox, and
    /// the per-dimension lines (with their "no effect on this platform") are
    /// skipped because `available` is false there.
    #[test]
    fn a_sandbox_switched_off_reads_as_disabled() {
        let workspace = tempfile::tempdir().unwrap();
        let off = DoctorReport::collect(
            workspace.path(),
            &AgentConfig {
                sandbox_mode: SandboxMode::Off,
                ..AgentConfig::default()
            },
        );
        let overall =
            Enforcement::weakest(off.sandbox.filesystem.clone(), off.sandbox.network.clone());
        assert_eq!(sandbox_state(&off, &overall), "disabled");
        assert!(!off.sandbox.available);

        let on = DoctorReport::collect(workspace.path(), &AgentConfig::default());
        let overall =
            Enforcement::weakest(on.sandbox.filesystem.clone(), on.sandbox.network.clone());
        assert_ne!(sandbox_state(&on, &overall), "disabled");
    }
}
