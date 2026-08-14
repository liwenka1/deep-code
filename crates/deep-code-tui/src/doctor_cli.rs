//! `deep-code doctor` command.

use deep_code_agent::{AgentConfig, DoctorReport, Enforcement};

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

pub fn run_doctor(json: bool) -> anyhow::Result<()> {
    let workspace = workspace_root();
    let loaded = AgentConfig::load(&workspace);
    let report =
        DoctorReport::collect(&workspace, &loaded.config).with_config_layers(&loaded.report);

    if json {
        println!("{}", report.to_json_pretty()?);
        return Ok(());
    }

    println!("deep-code doctor");
    println!("  version: {}", report.version);
    println!("  workspace: {}", report.workspace);
    println!(
        "  config: {} (present={})",
        report.config_path, report.config_present
    );
    if let Some(layers) = &report.config_layers {
        for layer in &layers.layers {
            match &layer.error {
                Some(error) => println!(
                    "    layer {}: {} (present={}, 错误: {error})",
                    layer.name, layer.path, layer.present
                ),
                None => println!(
                    "    layer {}: {} (present={})",
                    layer.name, layer.path, layer.present
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
            println!("    警告: {warning}");
        }
    }
    println!("  api key: {}", report.api_key.source);
    println!("  model: {} @ {}", report.default_model, report.base_url);
    println!(
        "  deepseek: auto_model={} reasoning={} currency={} beta={}",
        report.deepseek.auto_model,
        report.deepseek.reasoning_effort,
        report.deepseek.cost_currency,
        report.deepseek.beta_endpoint
    );
    for model in &report.deepseek.models {
        println!(
            "    - {} ctx={} reasoning={} tools={}",
            model.id, model.context_window, model.supports_reasoning, model.supports_tools
        );
    }
    if report.api_key.source == "missing" {
        println!("  api key 引导:\n{}", report.deepseek.api_key_hint);
    }
    // "available" is not the same as "enforcing": a backend can exist and still
    // confine nothing (Windows Job Object). Report what it actually does.
    let fully_enforcing = report.sandbox.filesystem.is_full() && report.sandbox.network.is_full();
    let sandbox_state = if !report.sandbox.available {
        "unavailable"
    } else if fully_enforcing {
        "enforcing"
    } else {
        "partial"
    };
    println!("  sandbox: {} ({})", sandbox_state, report.sandbox.detail);
    if report.sandbox.available && !fully_enforcing {
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
