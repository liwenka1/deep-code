//! MCP management commands for the `deep-code` CLI.

use std::path::PathBuf;

use deep_code_agent::{McpManager, load_mcp_config, set_server_enabled, workspace_mcp_config_path};

pub fn run_mcp_command(subcommand: &str, args: &[String]) -> anyhow::Result<()> {
    let workspace = workspace_root();
    match subcommand {
        "list" => cmd_list(&workspace),
        "validate" => cmd_validate(&workspace),
        "reload" => cmd_reload(&workspace),
        "enable" => cmd_set_enabled(&workspace, args, true),
        "disable" => cmd_set_enabled(&workspace, args, false),
        other => {
            anyhow::bail!("unknown mcp subcommand '{other}'. Use list, enable, disable, validate, or reload.");
        }
    }
}

fn cmd_list(workspace: &std::path::Path) -> anyhow::Result<()> {
    let manager = McpManager::load_from_workspace(workspace).unwrap_or_default();
    let config_path = config_path_for_display(workspace);
    println!("# MCP config: {}", config_path.display());
    let servers = manager.list_servers();
    if servers.is_empty() {
        println!("No MCP servers configured.");
        return Ok(());
    }
    for server in servers {
        println!(
            "{}\tenabled={}\tstatus={:?}\ttools={}\tresources={}\tprompts={}",
            server.name,
            server.enabled,
            server.status,
            server.tool_count,
            server.resource_count,
            server.prompt_count,
        );
    }
    Ok(())
}

fn cmd_validate(workspace: &std::path::Path) -> anyhow::Result<()> {
    let manager = McpManager::load_from_workspace(workspace).unwrap_or_default();
    let report = manager.validate();
    for server in &report.servers {
        println!(
            "{}: enabled={} status={:?} tools={}",
            server.name, server.enabled, server.status, server.tool_count
        );
    }
    for error in &report.errors {
        eprintln!("error: {error}");
    }
    if report.valid {
        println!("MCP validation passed.");
    } else {
        anyhow::bail!("MCP validation failed with {} error(s)", report.errors.len());
    }
    Ok(())
}

fn cmd_reload(workspace: &std::path::Path) -> anyhow::Result<()> {
    let configs = load_mcp_config(workspace)?.to_server_configs();
    let mut manager = McpManager::new();
    manager.reload_configs(configs)?;
    let report = manager.validate();
    println!(
        "Reloaded {} MCP server(s); {} ready.",
        report.servers.len(),
        report
            .servers
            .iter()
            .filter(|server| matches!(server.status, deep_code_agent::McpServerStatus::Ready))
            .count()
    );
    Ok(())
}

fn cmd_set_enabled(workspace: &std::path::Path, args: &[String], enabled: bool) -> anyhow::Result<()> {
    let Some(name) = args.first() else {
        anyhow::bail!("Usage: deep-code mcp {} <server>", if enabled { "enable" } else { "disable" });
    };
    if args.len() > 1 {
        anyhow::bail!("Usage: deep-code mcp {} <server>", if enabled { "enable" } else { "disable" });
    }
    let path = set_server_enabled(workspace, name, enabled).map_err(|error| anyhow::anyhow!(error))?;
    cmd_reload(workspace)?;
    println!(
        "MCP server '{name}' {} (saved to {}).",
        if enabled { "enabled" } else { "disabled" },
        path.display()
    );
    Ok(())
}

fn workspace_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("failed to resolve workspace: {error}");
        std::process::exit(1);
    })
}

fn config_path_for_display(workspace: &std::path::Path) -> std::path::PathBuf {
    let local = workspace_mcp_config_path(workspace);
    if local.is_file() {
        return local;
    }
    deep_code_agent::default_mcp_config_path()
}
