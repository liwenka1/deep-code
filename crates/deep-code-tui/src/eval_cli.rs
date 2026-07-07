//! `deep-code eval` command.
//!
//! Rollout only: produces official-format `predictions.json` (patches) plus a
//! local report. Scoring stays with the official SWE-bench harness — submit
//! the predictions via sb-cli to get the real resolved rate.

use std::path::PathBuf;

use deep_code_agent::AgentConfig;
use deep_code_eval::{EvalConfig, load_bench, run_bench};

use crate::cli::workspace_root;

#[allow(clippy::too_many_arguments)]
pub async fn run_eval(
    subset: String,
    split: String,
    sample: Option<usize>,
    parallel: usize,
    json: bool,
    markdown: bool,
    timeout_secs: u64,
    out_dir: PathBuf,
) -> anyhow::Result<()> {
    let workspace = workspace_root();

    // Load agent config from workspace (respects env vars, global config, etc.).
    let loaded = AgentConfig::load(&workspace);
    let agent_config = loaded.config;

    // A benchmark run against the offline echo backend is meaningless (and
    // wastes hours producing all-empty patches) — refuse instead of warning.
    if agent_config
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        anyhow::bail!(
            "未配置 DeepSeek API key,评测拒绝在离线 echo 后端上运行。\
先设置 DEEPSEEK_API_KEY 或在 ~/.deep-code/config.toml 配置。"
        );
    }

    let eval_config = EvalConfig {
        bench: "swe-bench".into(),
        subset: subset.clone(),
        split: split.clone(),
        sample,
        parallelism: parallel,
        agent_config,
        instance_timeout: std::time::Duration::from_secs(timeout_secs),
    };

    println!("Loading SWE-bench {subset}/{split} ...");
    let bench_set = load_bench("swe-bench", &subset, &split, sample).await?;
    println!(
        "Loaded {} instances (sample={})",
        bench_set.instances.len(),
        sample.map_or("all".into(), |n| n.to_string())
    );
    println!();

    println!("Running rollout (parallel={parallel}, timeout={timeout_secs}s/instance) ...");
    let report = run_bench(eval_config, &bench_set).await?;

    // Always persist the two artifacts: official predictions + full report.
    std::fs::create_dir_all(&out_dir)?;
    let predictions_path = out_dir.join("predictions.json");
    std::fs::write(
        &predictions_path,
        deep_code_eval::report::to_predictions_json(&report)?,
    )?;
    let report_path = out_dir.join("report.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;

    println!();
    if json {
        deep_code_eval::report::print_json(&report)?;
    } else if markdown {
        println!("{}", deep_code_eval::report::to_markdown(&report));
    } else {
        deep_code_eval::report::print_summary(&report);
    }

    let sb_subset = match subset.as_str() {
        "verified" => "swe-bench_verified",
        _ => "swe-bench_lite",
    };
    println!("已写出:");
    println!("  {}", predictions_path.display());
    println!("  {}", report_path.display());
    println!();
    println!("下一步(官方评分,得出真实 resolved 率):");
    println!(
        "  sb-cli submit {sb_subset} {split} --predictions_path {} --run_id <run_id>",
        predictions_path.display()
    );
    Ok(())
}
