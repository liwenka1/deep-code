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
