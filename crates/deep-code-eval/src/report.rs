//! Report output (JSON, terminal, markdown).

use crate::runner::{BenchReport, InstanceStatus};

/// Write the report as pretty JSON to stdout.
pub fn print_json(report: &BenchReport) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

/// Print a human-readable summary table to stdout.
pub fn print_summary(report: &BenchReport) {
    println!();
    println!("═══════════════════════════════════════════════");
    println!("  Benchmark:    {} / {}", report.bench, report.subset);
    println!("  Started at:   {}", report.started_at);
    println!("  Duration:     {:.1}s", report.duration_ms as f64 / 1000.0);
    println!("───────────────────────────────────────────────");
    println!("  Total:        {}", report.total);
    println!("  ✅ Resolved:   {}", report.resolved);
    println!("  ❌ Unresolved: {}", report.unresolved);
    println!("  ⏱️  Timeouts:   {}", report.timeouts);
    println!("  💥 Errors:     {}", report.errors);
    println!("───────────────────────────────────────────────");
    if report.total > 0 {
        let rate = report.resolved as f64 / report.total as f64 * 100.0;
        println!("  Resolve rate: {:.1}%", rate);
    }
    println!("═══════════════════════════════════════════════");
    println!();

    // Print per-instance details.
    for r in &report.results {
        let status_icon = match r.status {
            InstanceStatus::Resolved => "✅",
            InstanceStatus::Unresolved => "❌",
            InstanceStatus::Timeout => "⏱️",
            InstanceStatus::Error => "💥",
        };
        let patch_size = r.patch.len();
        println!("  {status_icon} {}  ({}ms, patch={patch_size}b){}",
            r.instance_id,
            r.duration_ms,
            r.error.as_ref().map(|e| format!("  error={e}")).unwrap_or_default(),
        );
    }
}

/// Generate a markdown report suitable for README.
pub fn to_markdown(report: &BenchReport) -> String {
    let rate = if report.total > 0 {
        format!("{:.1}%", report.resolved as f64 / report.total as f64 * 100.0)
    } else {
        "N/A".into()
    };

    let mut md = String::new();
    md.push_str("## SWE-bench 评测报告\n\n");
    md.push_str("| 指标 | 值 |\n");
    md.push_str("|------|-----|\n");
    md.push_str(&format!("| 数据集 | {} / {} |\n", report.bench, report.subset));
    md.push_str(&format!("| 实例数 | {} |\n", report.total));
    md.push_str(&format!("| 解决数 | {} / {} |\n", report.resolved, report.total));
    md.push_str(&format!("| 通过率 | **{rate}** |\n"));
    md.push_str(&format!("| 耗时 | {:.1}s |\n", report.duration_ms as f64 / 1000.0));
    md.push_str("\n### 实例详情\n\n");
    md.push_str("| 实例 | 状态 | 耗时 | Patch |\n");
    md.push_str("|------|------|------|-------|\n");
    for r in &report.results {
        let status_str = match r.status {
            InstanceStatus::Resolved => "✅",
            InstanceStatus::Unresolved => "❌",
            InstanceStatus::Timeout => "⏱️",
            InstanceStatus::Error => "💥",
        };
        md.push_str(&format!(
            "| {} | {} | {}ms | {}b |\n",
            r.instance_id, status_str, r.duration_ms, r.patch.len()
        ));
    }
    md
}
