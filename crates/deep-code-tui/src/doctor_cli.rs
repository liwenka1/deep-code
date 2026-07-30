//! `deep-code doctor` command.

use deep_code_agent::{AgentConfig, DoctorReport};

use crate::cli::workspace_root;

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
    let sandbox_state = match (
        report.sandbox.available,
        report.sandbox.confines_filesystem,
        report.sandbox.confines_network,
    ) {
        (false, _, _) => "unavailable",
        (true, true, true) => "enforcing",
        _ => "partial",
    };
    println!("  sandbox: {} ({})", sandbox_state, report.sandbox.detail);
    if report.sandbox.available
        && !(report.sandbox.confines_filesystem && report.sandbox.confines_network)
    {
        println!(
            "    workspace-write confinement: {}",
            if report.sandbox.confines_filesystem {
                "yes"
            } else {
                "NO"
            }
        );
        println!(
            "    network withheld by default: {}",
            if report.sandbox.confines_network {
                "yes"
            } else {
                "NO"
            }
        );
        println!("    → [sandbox] network has no effect on this platform.");
    }
    println!("  skills: {} loaded", report.skills.total_count);
    Ok(())
}
