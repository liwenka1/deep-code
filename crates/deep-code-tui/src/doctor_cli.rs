//! `deep-code doctor` command.

use deep_code_agent::{AgentConfig, DoctorReport};

use crate::cli::workspace_root;

pub fn run_doctor(json: bool) -> anyhow::Result<()> {
    let workspace = workspace_root();
    let config = AgentConfig::from_env();
    let report = DoctorReport::collect(&workspace, &config);

    if json {
        println!("{}", report.to_json_pretty()?);
        return Ok(());
    }

    println!("deep-code doctor");
    println!("  version: {}", report.version);
    println!("  workspace: {}", report.workspace);
    println!("  config: {} (present={})", report.config_path, report.config_present);
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
    println!(
        "  sandbox: {} ({})",
        if report.sandbox.available { "available" } else { "unavailable" },
        report.sandbox.detail
    );
    println!(
        "  mcp: {} servers (config present={})",
        report.mcp.servers.len(),
        report.mcp.present
    );
    for server in &report.mcp.servers {
        println!(
            "    - {} enabled={} status={} tools={}",
            server.name, server.enabled, server.status, server.tool_count
        );
    }
    println!("  skills: {} loaded", report.skills.total_count);
    println!(
        "  hooks: {} (present={})",
        report.hooks.config_path, report.hooks.present
    );
    Ok(())
}
