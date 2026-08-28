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
                    "    layer {}: {} (present={}, 错误: {})",
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
            println!("    警告: {}", clean(warning));
        }
    }
    println!("  api key: {}", report.api_key.source);
    println!(
        "  model: {} @ {}",
        clean(&report.default_model),
        clean(&report.base_url)
    );
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
    // One definition of "what does this host enforce overall", shared with the
    // approval panel and the tool descriptions. Deriving it here by hand meant
    // two, and they had drifted: a Windows host (a backend that exists and
    // confines nothing) printed `partial`, claiming a boundary with holes where
    // there is no boundary at all — while the two lines below it said `NO`.
    let overall = Enforcement::weakest(
        report.sandbox.filesystem.clone(),
        report.sandbox.network.clone(),
    );
    let sandbox_state = if !report.sandbox.available {
        "unavailable"
    } else {
        match overall {
            Enforcement::Full => "enforcing",
            Enforcement::Partial { .. } => "partial",
            Enforcement::None => "not enforcing",
        }
    };
    println!("  sandbox: {} ({})", sandbox_state, report.sandbox.detail);
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
