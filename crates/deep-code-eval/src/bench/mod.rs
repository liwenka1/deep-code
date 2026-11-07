//! Benchmark data source trait and SWE-bench implementation.

mod swe_bench;

use serde::Deserialize;

/// A single evaluation instance from a benchmark.
pub trait BenchmarkInstance: std::fmt::Debug {
    /// Unique identifier (e.g. "django__django-16899").
    fn instance_id(&self) -> &str;
    /// Problem statement / issue description fed to the agent.
    fn problem_statement(&self) -> &str;
    /// Repository name (e.g. "django/django").
    fn repo(&self) -> &str;
    /// Base commit SHA to check out before running the agent.
    fn base_commit(&self) -> &str;
}

/// A benchmark data set.
#[derive(Debug, Clone)]
pub struct BenchmarkSet<T: BenchmarkInstance + Clone> {
    pub instances: Vec<T>,
}

/// Load a benchmark by name.
pub async fn load_bench(
    bench: &str,
    subset: &str,
    split: &str,
    sample: Option<usize>,
) -> anyhow::Result<BenchmarkSet<SweBenchInstance>> {
    match bench {
        "swe-bench" => swe_bench::load(subset, split, sample).await,
        other => anyhow::bail!("unknown benchmark '{other}' (supported: swe-bench)"),
    }
}

// ── SWE-bench ────────────────────────────────────────────────────────────────

/// The fields consumed from a SWE-bench datasets-server response.
#[derive(Debug, Clone, Deserialize)]
pub struct SweBenchInstance {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
}

impl BenchmarkInstance for SweBenchInstance {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
    fn problem_statement(&self) -> &str {
        &self.problem_statement
    }
    fn repo(&self) -> &str {
        &self.repo
    }
    fn base_commit(&self) -> &str {
        &self.base_commit
    }
}
