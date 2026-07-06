//! Evaluation runner: drives the agent against each benchmark instance.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deep_code_agent::{
    AgentConfig, ApprovalDecision, LaunchedRuntime,
    launch_runtime,
};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::bench::{BenchmarkInstance, BenchmarkSet};

// ── Configuration ────────────────────────────────────────────────────────────

/// Evaluation configuration.
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// Which benchmark to run ("swe-bench").
    pub bench: String,
    /// Subset within the benchmark ("lite", "verified").
    pub subset: String,
    /// Limit number of instances (None = all).
    pub sample: Option<usize>,
    /// Concurrency (how many instances to run in parallel).
    pub parallelism: usize,
    /// Agent configuration (reused for all instances).
    pub agent_config: AgentConfig,
    /// Workspace root for config loading.
    pub workspace_root: std::path::PathBuf,
    /// Timeout per instance (wall-clock).
    pub instance_timeout: Duration,
    /// Output directory for results JSON.
    pub output_dir: Option<std::path::PathBuf>,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            bench: "swe-bench".into(),
            subset: "lite".into(),
            sample: None,
            parallelism: 1,
            agent_config: AgentConfig::default(),
            workspace_root: std::path::PathBuf::from("."),
            instance_timeout: Duration::from_secs(300),
            output_dir: None,
        }
    }
}

// ── Results ──────────────────────────────────────────────────────────────────

/// Result of evaluating one instance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceResult {
    pub instance_id: String,
    pub status: InstanceStatus,
    /// Git diff patch produced by the agent (empty if not resolved).
    pub patch: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Error message if failed.
    pub error: Option<String>,
}

/// Status of a single instance evaluation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InstanceStatus {
    /// Agent produced a non-empty diff.
    Resolved,
    /// Agent finished but produced no diff.
    Unresolved,
    /// Agent timed out.
    Timeout,
    /// Error during setup or execution.
    Error,
}

/// Full benchmark report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BenchReport {
    pub bench: String,
    pub subset: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub total: usize,
    pub resolved: usize,
    pub unresolved: usize,
    pub timeouts: usize,
    pub errors: usize,
    pub results: Vec<InstanceResult>,
}

// ── Runner ───────────────────────────────────────────────────────────────────

/// Run a benchmark.
pub async fn run_bench(config: EvalConfig, bench_set: &BenchmarkSet<impl BenchmarkInstance + Clone + Send + Sync + 'static>) -> anyhow::Result<BenchReport> {
    let started_at = chrono_now();
    let start = Instant::now();

    let instances: Vec<_> = bench_set.instances.clone();
    let semaphore = Arc::new(Semaphore::new(config.parallelism));
    let results = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(instances.len())));

    let mut handles = Vec::with_capacity(instances.len());
    for instance in instances {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let config = config.clone();
        let results = results.clone();

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let result = run_single(&config, &instance).await;
            let mut guard = results.lock().await;
            guard.push(result);
        });
        handles.push(handle);
    }

    // Wait for all instances.
    for handle in handles {
        let _ = handle.await;
    }

    let elapsed = start.elapsed();
    let final_results = results.lock().await.clone();

    let resolved = final_results.iter().filter(|r| r.status == InstanceStatus::Resolved).count();
    let unresolved = final_results.iter().filter(|r| r.status == InstanceStatus::Unresolved).count();
    let timeouts = final_results.iter().filter(|r| r.status == InstanceStatus::Timeout).count();
    let errors = final_results.iter().filter(|r| r.status == InstanceStatus::Error).count();

    Ok(BenchReport {
        bench: config.bench,
        subset: config.subset,
        started_at,
        duration_ms: elapsed.as_millis() as u64,
        total: final_results.len(),
        resolved,
        unresolved,
        timeouts,
        errors,
        results: final_results,
    })
}

/// Run the agent on a single benchmark instance.
async fn run_single(config: &EvalConfig, instance: &impl BenchmarkInstance) -> InstanceResult {
    let instance_id = instance.instance_id().to_string();
    println!("  ▶ {instance_id} ...");

    let start = Instant::now();

    // Create a temp workspace directory.
    let workspace = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            return InstanceResult {
                instance_id,
                status: InstanceStatus::Error,
                patch: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("failed to create temp dir: {e}")),
            };
        }
    };

    // Clone repository at base commit.
    if let Err(e) = clone_repo(instance.repo(), instance.base_commit(), workspace.path()).await {
        return InstanceResult {
            instance_id,
            status: InstanceStatus::Error,
            patch: String::new(),
            duration_ms: start.elapsed().as_millis() as u64,
            error: Some(format!("clone failed: {e}")),
        };
    }

    // Launch the agent runtime (non-fallible).
    let launched = launch_runtime(
        &config.agent_config,
        workspace.path().to_path_buf(),
        None,
    );

    // Submit the issue as a user prompt.
    let prompt = instance.problem_statement();
    let receiver = launched.handle.submit_user(prompt.to_string()).await;

    // Consume events with a timeout.
    let consume = async {
        consume_events(&launched, receiver).await;
    };

    let timed_out = timeout(config.instance_timeout, consume).await.is_err();

    // Extract patch.
    let patch = match extract_git_diff(workspace.path()) {
        Ok(p) => p,
        Err(e) => {
            launched.shutdown().await;
            return InstanceResult {
                instance_id,
                status: InstanceStatus::Error,
                patch: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("patch extraction failed: {e}")),
            };
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    launched.shutdown().await;

    let (status, error) = if timed_out {
        (InstanceStatus::Timeout, Some("instance timeout".into()))
    } else if patch.is_empty() {
        (InstanceStatus::Unresolved, None)
    } else {
        (InstanceStatus::Resolved, None)
    };

    println!("  ✓ {instance_id}: {status:?} ({duration_ms}ms, patch={}b)", patch.len());
    InstanceResult { instance_id, status, patch, duration_ms, error }
}

/// Consume all events from the runtime until the turn finishes.
/// Auto-approves any approval requests.
async fn consume_events(launched: &LaunchedRuntime, mut receiver: deep_code_agent::RuntimeEventReceiver) {
    use deep_code_agent::RuntimeEvent;

    loop {
        let event = match receiver.recv().await {
            Some(e) => e,
            None => break,
        };

        match &event {
            RuntimeEvent::ApprovalRequired { .. } => {
                launched.handle.submit_approval(ApprovalDecision::Approved).await;
            }
            RuntimeEvent::TurnFinished { .. } | RuntimeEvent::TurnCancelled { .. } => break,
            RuntimeEvent::Error { .. } => break,
            _ => {}
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Clone a git repository at a specific commit.
/// Uses modern git's ability to fetch a single commit without full history.
async fn clone_repo(repo: &str, commit: &str, dest: &Path) -> anyhow::Result<()> {
    let url = format!("https://github.com/{repo}.git");

    // Create empty repo
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("git init failed: {e}"))?;

    if !status.success() {
        anyhow::bail!("git init failed");
    }

    // Add remote
    let status = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["remote", "add", "origin", &url])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("git remote add failed");
    }

    // Fetch only the specific commit (shallow)
    let status = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["fetch", "--depth", "1", "origin", commit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("git fetch commit '{commit}' failed (may not exist or network issue)");
    }

    // Checkout FETCH_HEAD (which points to the fetched commit)
    let status = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["checkout", "FETCH_HEAD"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("git checkout FETCH_HEAD failed");
    }

    Ok(())
}

/// Extract a git diff from the workspace (unstaged + staged changes).
fn extract_git_diff(workspace: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff", "--staged"])
        .output()?;
    let mut patch = String::from_utf8_lossy(&output.stdout).to_string();

    let unstaged = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff"])
        .output()?;
    patch.push_str(&String::from_utf8_lossy(&unstaged.stdout));

    Ok(patch)
}

/// Get current timestamp as ISO string.
fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let _days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}Z")
}
