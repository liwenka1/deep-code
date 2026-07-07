//! Report output (JSON, terminal, markdown) and official predictions export.
//!
//! Wording is deliberate: this crate produces PATCHES, not scores. "patch
//! produced" is an unscored rollout metric; the real resolved rate only
//! exists after the official SWE-bench harness (sb-cli) evaluates the
//! predictions file.

use crate::runner::{BenchReport, InstanceStatus};

/// Write the report as pretty JSON to stdout.
pub fn print_json(report: &BenchReport) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

/// Official SWE-bench predictions payload for one instance.
#[derive(serde::Serialize)]
struct Prediction<'a> {
    instance_id: &'a str,
    model_name_or_path: &'a str,
    model_patch: &'a str,
}

/// Render the official predictions JSON (the file sb-cli consumes).
pub fn to_predictions_json(report: &BenchReport) -> anyhow::Result<String> {
    let predictions: Vec<Prediction<'_>> = report
        .results
        .iter()
        .map(|r| Prediction {
            instance_id: &r.instance_id,
            model_name_or_path: "deep-code",
            model_patch: &r.patch,
        })
        .collect();
    Ok(serde_json::to_string_pretty(&predictions)?)
}

fn status_icon(status: &InstanceStatus) -> &'static str {
    match status {
        InstanceStatus::PatchProduced => "📦",
        InstanceStatus::EmptyPatch => "∅",
        InstanceStatus::Timeout => "⏱",
        InstanceStatus::Error => "💥",
    }
}

/// Print a human-readable summary table to stdout.
pub fn print_summary(report: &BenchReport) {
    println!();
    println!("═══════════════════════════════════════════════");
    println!(
        "  Benchmark:      {} {} / {} (未评分 rollout)",
        report.bench, report.subset, report.split
    );
    println!("  Started at:     {}", report.started_at);
    println!(
        "  Duration:       {:.1}s",
        report.duration_ms as f64 / 1000.0
    );
    println!("───────────────────────────────────────────────");
    println!("  Total:          {}", report.total);
    println!("  📦 有 patch:    {}", report.patches_produced);
    println!("  ∅  空 patch:    {}", report.empty_patches);
    println!("  ⏱  超时:        {}", report.timeouts);
    println!("  💥 错误:        {}", report.errors);
    println!("  💰 总成本:      ¥{:.4}", report.total_cost_cny);
    println!("───────────────────────────────────────────────");
    println!("  注意:patch 产出 ≠ 解决。真实 resolved 率需将");
    println!("  predictions.json 提交官方评测(sb-cli)后得出。");
    println!("═══════════════════════════════════════════════");
    println!();

    for r in &report.results {
        let route = match (&r.model, &r.route_source) {
            (Some(model), Some(source)) => format!(", {model}/{source}"),
            (Some(model), None) => format!(", {model}"),
            _ => String::new(),
        };
        println!(
            "  {} {}  ({}s, patch={}b, ¥{:.4}{route}{}){}",
            status_icon(&r.status),
            r.instance_id,
            r.duration_ms / 1000,
            r.patch.len(),
            r.cost_cny,
            if r.cascade_triggered {
                ", ⚡cascade"
            } else {
                ""
            },
            r.error
                .as_ref()
                .map(|e| format!("  error={e}"))
                .unwrap_or_default(),
        );
    }
}

/// Generate a markdown rollout report. Deliberately contains no
/// "resolve rate": that number does not exist until official scoring.
pub fn to_markdown(report: &BenchReport) -> String {
    let mut md = String::new();
    md.push_str("## SWE-bench rollout 报告(未评分)\n\n");
    md.push_str(
        "> patch 产出 ≠ 解决。真实 resolved 率需将 predictions.json \
提交官方评测(sb-cli)后得出。\n\n",
    );
    md.push_str("| 指标 | 值 |\n|------|-----|\n");
    md.push_str(&format!(
        "| 数据集 | {} {} / {} |\n",
        report.bench, report.subset, report.split
    ));
    md.push_str(&format!("| 实例数 | {} |\n", report.total));
    md.push_str(&format!(
        "| 产出 patch | {} / {} |\n",
        report.patches_produced, report.total
    ));
    md.push_str(&format!(
        "| 超时 / 错误 | {} / {} |\n",
        report.timeouts, report.errors
    ));
    md.push_str(&format!("| 总成本 | ¥{:.4} |\n", report.total_cost_cny));
    md.push_str(&format!(
        "| 耗时 | {:.1}s |\n",
        report.duration_ms as f64 / 1000.0
    ));
    md.push_str("\n### 实例详情\n\n");
    md.push_str("| 实例 | 状态 | 耗时 | Patch | 成本 | 路由 |\n");
    md.push_str("|------|------|------|-------|------|------|\n");
    for r in &report.results {
        md.push_str(&format!(
            "| {} | {} | {}s | {}b | ¥{:.4} | {}{} |\n",
            r.instance_id,
            status_icon(&r.status),
            r.duration_ms / 1000,
            r.patch.len(),
            r.cost_cny,
            r.model.as_deref().unwrap_or("-"),
            if r.cascade_triggered { " ⚡" } else { "" },
        ));
    }
    md
}
