//! `deep-code eval` command.
//!
//! Runs a benchmark evaluation and outputs the results.

use deep_code_agent::AgentConfig;
use deep_code_eval::{EvalConfig, load_bench, run_bench};

use crate::cli::workspace_root;

pub async fn run_eval(
    subset: String,
    sample: Option<usize>,
    parallel: usize,
    json: bool,
    markdown: bool,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let workspace = workspace_root();

    // Load agent config from workspace (respects env vars, global config, etc.).
    let loaded = AgentConfig::load(&workspace);
    let agent_config = loaded.config;

    // Check if API key is available.
    let has_api_key = agent_config.api_key.is_some()
        || std::env::var("DEEPSEEK_API_KEY").ok().is_some_and(|k| !k.trim().is_empty());

    if !has_api_key {
        eprintln!("Warning: No DeepSeek API key configured. The agent will run in echo mode (no real LLM calls).");
        eprintln!("Set DEEPSEEK_API_KEY environment variable or configure it in ~/.deep-code/config.toml");
        println!();
    }

    // Build eval config.
    let eval_config = EvalConfig {
        bench: "swe-bench".into(),
        subset: subset.clone(),
        sample,
        parallelism: parallel,
        agent_config,
        workspace_root: workspace.clone(),
        instance_timeout: std::time::Duration::from_secs(timeout_secs),
        output_dir: None,
    };

    // Load benchmark dataset.
    println!("Loading SWE-bench/{subset} dataset ...");
    let bench_set = load_bench("swe-bench", &subset, sample).await?;
    println!(
        "Loaded {} instances (subset={subset}, sample={})",
        bench_set.instances.len(),
        sample.map_or("all".into(), |n| n.to_string())
    );
    println!();

    // Run evaluation.
    println!("Running evaluation (parallel={parallel}, timeout={timeout_secs}s) ...");
    let report = run_bench(eval_config, &bench_set).await?;

    // Output.
    println!();
    if json {
        deep_code_eval::report::print_json(&report)?;
    } else if markdown {
        let md = deep_code_eval::report::to_markdown(&report);
        println!("{md}");
    } else {
        deep_code_eval::report::print_summary(&report);
    }

    Ok(())
}
