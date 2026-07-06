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
    /// Hints (optional) — not sent to the agent; used for metadata.
    fn hints(&self) -> Option<&str>;
}

/// A benchmark data set.
#[derive(Debug, Clone)]
pub struct BenchmarkSet<T: BenchmarkInstance + Clone> {
    pub name: String,
    pub description: String,
    pub instances: Vec<T>,
}

/// Load a benchmark by name.
pub async fn load_bench(bench: &str, subset: &str, sample: Option<usize>) -> anyhow::Result<BenchmarkSet<SweBenchInstance>> {
    match bench {
        "swe-bench" => swe_bench::load(subset, sample).await,
        other => anyhow::bail!("unknown benchmark '{other}' (supported: swe-bench)"),
    }
}

// ── SWE-bench ────────────────────────────────────────────────────────────────

/// A single SWE-bench instance — matches the datasets-server API response.
#[derive(Debug, Clone, Deserialize)]
pub struct SweBenchInstance {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
    /// Populated from `hints_text` in the raw API response.
    #[serde(default, alias = "hints_text")]
    pub hints: Option<String>,
    /// Patch (reference solution, not used in Phase 1).
    #[serde(default)]
    pub patch: Option<String>,
    /// Test patch (reference test changes).
    #[serde(default)]
    pub test_patch: Option<String>,
    /// FAIL_TO_PASS test list (JSON string array).
    #[serde(default)]
    pub fail_to_pass: Option<String>,
    /// PASS_TO_PASS test list (JSON string array).
    #[serde(default)]
    pub pass_to_pass: Option<String>,
    /// When the issue was created.
    #[serde(default)]
    pub created_at: Option<String>,
    /// SWE-bench dataset version.
    #[serde(default)]
    pub version: Option<String>,
    /// Environment setup commit for Docker evaluation.
    #[serde(default)]
    pub environment_setup_commit: Option<String>,
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
    fn hints(&self) -> Option<&str> {
        self.hints.as_deref()
    }
}
