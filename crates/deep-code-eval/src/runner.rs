//! Evaluation runner: drives the agent against each benchmark instance and
//! produces official-format predictions (patches). Scoring is NOT done here —
//! a non-empty patch is not "resolved"; submit the predictions to the official
//! SWE-bench harness (sb-cli) for the real resolved rate.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deep_code_agent::{
    AgentConfig, ApprovalDecision, LaunchedRuntime, RuntimeEvent, RuntimeEventReceiver,
    TurnTelemetry, launch_runtime,
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
    /// Dataset split ("dev", "test").
    pub split: String,
    /// Limit number of instances (None = all).
    pub sample: Option<usize>,
    /// Concurrency (how many instances to run in parallel).
    pub parallelism: usize,
    /// Agent configuration (reused for all instances).
    pub agent_config: AgentConfig,
    /// Timeout per instance (wall-clock).
    pub instance_timeout: Duration,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            bench: "swe-bench".into(),
            subset: "lite".into(),
            split: "dev".into(),
            sample: None,
            parallelism: 1,
            agent_config: AgentConfig::default(),
            instance_timeout: Duration::from_secs(300),
        }
    }
}

// ── Results ──────────────────────────────────────────────────────────────────

/// Result of evaluating one instance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceResult {
    pub instance_id: String,
    pub status: InstanceStatus,
    /// Git diff produced by the agent (the SWE-bench `model_patch`).
    pub patch: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Session cost in CNY (from turn telemetry; 0 if unavailable).
    pub cost_cny: f64,
    /// Effective model of the turn (e.g. deepseek-v4-flash), if reported.
    pub model: Option<String>,
    /// What decided the route (heuristic / hard-rule / cascade).
    pub route_source: Option<String>,
    /// Whether cascade escalation latched during this instance.
    pub cascade_triggered: bool,
    /// Error message if failed.
    pub error: Option<String>,
}

/// Status of a single instance run. Deliberately NOT "resolved": whether a
/// patch actually fixes the issue is only known after official evaluation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InstanceStatus {
    /// Agent finished and produced a non-empty diff (unscored).
    PatchProduced,
    /// Agent finished but produced no diff.
    EmptyPatch,
    /// Agent hit the wall-clock timeout (partial diff may still be captured).
    Timeout,
    /// Error during setup or execution.
    Error,
}

/// Full rollout report (unscored — see [`InstanceStatus`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BenchReport {
    pub bench: String,
    pub subset: String,
    pub split: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub total: usize,
    pub patches_produced: usize,
    pub empty_patches: usize,
    pub timeouts: usize,
    pub errors: usize,
    pub total_cost_cny: f64,
    pub results: Vec<InstanceResult>,
}

// ── Runner ───────────────────────────────────────────────────────────────────

/// Run a benchmark rollout: produce patches for every instance.
pub async fn run_bench(
    config: EvalConfig,
    bench_set: &BenchmarkSet<impl BenchmarkInstance + Clone + Send + Sync + 'static>,
) -> anyhow::Result<BenchReport> {
    // Eval blind-approves every tool call over untrusted checkouts; without an
    // OS sandbox that means model-generated commands run bare on this machine.
    // Refuse instead of silently degrading.
    anyhow::ensure!(
        deep_code_agent::sandbox_available(),
        "refusing to run eval without an OS sandbox: eval auto-approves model \
         commands on untrusted repos, and this machine has no usable sandbox \
         backend. Run inside a container, or on macOS/Linux with sandbox support."
    );
    let started_at = utc_now_iso();
    let start = Instant::now();

    let instances: Vec<_> = bench_set.instances.clone();
    let semaphore = Arc::new(Semaphore::new(config.parallelism.max(1)));
    let results = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(instances.len())));

    let mut handles = Vec::with_capacity(instances.len());
    for instance in instances {
        let permit = semaphore.clone().acquire_owned().await?;
        let config = config.clone();
        let results = Arc::clone(&results);

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let result = run_single(&config, &instance).await;
            results.lock().await.push(result);
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }

    let mut final_results = results.lock().await.clone();
    // Completion order is nondeterministic under parallelism; sort for stable
    // reports and diffable predictions.
    final_results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let count =
        |status: InstanceStatus| final_results.iter().filter(|r| r.status == status).count();
    Ok(BenchReport {
        bench: config.bench,
        subset: config.subset,
        split: config.split,
        started_at,
        duration_ms: start.elapsed().as_millis() as u64,
        total: final_results.len(),
        patches_produced: count(InstanceStatus::PatchProduced),
        empty_patches: count(InstanceStatus::EmptyPatch),
        timeouts: count(InstanceStatus::Timeout),
        errors: count(InstanceStatus::Error),
        total_cost_cny: final_results.iter().map(|r| r.cost_cny).sum(),
        results: final_results,
    })
}

/// Task framing around the raw issue text: without it, many issue reports read
/// as questions and the agent answers in prose instead of editing code.
fn instance_prompt(instance: &impl BenchmarkInstance) -> String {
    format!(
        "You are working inside a git checkout of {repo}. Solve the GitHub issue \
below by editing the repository source code.\n\
Requirements:\n\
- Fix the root cause with a minimal change.\n\
- Do NOT modify any test files.\n\
- Do not run `git commit`; leave your edits in the working tree.\n\n\
<issue>\n{issue}\n</issue>",
        repo = instance.repo(),
        issue = instance.problem_statement(),
    )
}

/// Run the agent on a single benchmark instance.
async fn run_single(config: &EvalConfig, instance: &impl BenchmarkInstance) -> InstanceResult {
    let instance_id = instance.instance_id().to_string();
    println!("  ▶ {instance_id} ...");
    let start = Instant::now();

    let error_result = |error: String, start: &Instant| InstanceResult {
        instance_id: instance.instance_id().to_string(),
        status: InstanceStatus::Error,
        patch: String::new(),
        duration_ms: start.elapsed().as_millis() as u64,
        cost_cny: 0.0,
        model: None,
        route_source: None,
        cascade_triggered: false,
        error: Some(error),
    };

    let workspace = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => return error_result(format!("failed to create temp dir: {e}"), &start),
    };
    if let Err(e) = checkout_repo(instance.repo(), instance.base_commit(), workspace.path()).await {
        return error_result(format!("checkout failed: {e}"), &start);
    }

    let launched = launch_runtime(&config.agent_config, workspace.path().to_path_buf(), None);
    for warning in &launched.warnings {
        eprintln!("warning: {warning}");
    }
    let receiver = launched.handle.submit_user(instance_prompt(instance)).await;

    let outcome = timeout(config.instance_timeout, consume_events(&launched, receiver)).await;
    let (timed_out, turn) = match outcome {
        Ok(turn) => (false, turn),
        Err(_) => {
            // Stop the still-running turn before reading the working tree,
            // otherwise the diff races against in-flight edits.
            let cancel_rx = launched.handle.cancel_turn().await;
            let _ = timeout(Duration::from_secs(10), drain(cancel_rx)).await;
            (true, TurnOutcome::default())
        }
    };
    // Fully stop the runtime before extracting the diff.
    launched.shutdown().await;

    let patch = match extract_git_diff(workspace.path()).await {
        Ok(patch) => patch,
        Err(e) => return error_result(format!("patch extraction failed: {e}"), &start),
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let (status, error) = if timed_out {
        (InstanceStatus::Timeout, Some("instance timeout".into()))
    } else if let Some(message) = turn.error {
        (InstanceStatus::Error, Some(message))
    } else if patch.trim().is_empty() {
        (InstanceStatus::EmptyPatch, None)
    } else {
        (InstanceStatus::PatchProduced, None)
    };

    let (cost_cny, model, route_source, cascade_triggered) = match &turn.telemetry {
        Some(t) => (
            t.session_cost.cny,
            Some(t.effective_model.clone()),
            Some(t.route_source.clone()),
            t.cascade_triggered,
        ),
        None => (0.0, None, None, false),
    };
    println!(
        "  ✓ {instance_id}: {status:?} ({}s, patch={}b, ¥{cost_cny:.4})",
        duration_ms / 1000,
        patch.len()
    );
    InstanceResult {
        instance_id,
        status,
        patch,
        duration_ms,
        cost_cny,
        model,
        route_source,
        cascade_triggered,
        error,
    }
}

#[derive(Default)]
struct TurnOutcome {
    telemetry: Option<TurnTelemetry>,
    error: Option<String>,
}

/// Consume runtime events until the turn terminates. Approvals are granted
/// automatically; `submit_approval` returns a NEW event receiver which MUST
/// replace the old one (the previous channel closes at the approval point).
async fn consume_events(
    launched: &LaunchedRuntime,
    mut receiver: RuntimeEventReceiver,
) -> TurnOutcome {
    let mut outcome = TurnOutcome::default();
    loop {
        let Some(event) = receiver.recv().await else {
            // Channel closed without a terminal event (should not happen).
            outcome
                .error
                .get_or_insert_with(|| "event stream ended without TurnFinished".into());
            return outcome;
        };
        match event {
            RuntimeEvent::ApprovalRequired { .. } => {
                receiver = launched
                    .handle
                    .submit_approval(ApprovalDecision::Approved)
                    .await;
            }
            RuntimeEvent::TurnFinished { telemetry, .. } => {
                outcome.telemetry = telemetry;
                return outcome;
            }
            RuntimeEvent::TurnCancelled { .. } => {
                outcome.error = Some("turn cancelled".into());
                return outcome;
            }
            RuntimeEvent::Error { message, .. } => {
                outcome.error = Some(message);
                return outcome;
            }
            _ => {}
        }
    }
}

/// Drain a receiver until it closes (used after cancel_turn).
async fn drain(mut receiver: RuntimeEventReceiver) {
    while receiver.recv().await.is_some() {}
}

// ── Repo checkout with a per-repo cache ─────────────────────────────────────

/// Bare-clone cache: SWE-bench reuses the same repos for many instances
/// (django alone is ~1/3 of Lite); re-fetching per instance wastes GBs.
fn cache_dir() -> PathBuf {
    // Same HOME→USERPROFILE fallback the config layers use (Windows-safe).
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache")
        .join("deep-code")
        .join("swebench-repos")
}

async fn git(args: &[&str]) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("git")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

/// Check out `repo` at `commit` into `dest`, going through the bare cache.
async fn checkout_repo(repo: &str, commit: &str, dest: &Path) -> anyhow::Result<()> {
    let cache = cache_dir().join(repo.replace('/', "__"));
    let cache_str = cache.to_string_lossy().into_owned();
    let dest_str = dest.to_string_lossy().into_owned();
    let url = format!("https://github.com/{repo}.git");

    if !cache.exists() {
        if let Some(parent) = cache.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        git(&["clone", "--bare", &url, &cache_str]).await?;
    }
    // `--shared` borrows objects from the cache instead of copying; workdirs
    // are throwaway temp dirs, so the alternates coupling is fine.
    git(&["clone", "--shared", "--no-checkout", &cache_str, &dest_str]).await?;
    if git(&["-C", &dest_str, "checkout", "-q", commit])
        .await
        .is_err()
    {
        // Cache may predate the commit: refresh it once and retry.
        git(&[
            "-C",
            &cache_str,
            "fetch",
            "origin",
            "+refs/heads/*:refs/heads/*",
        ])
        .await?;
        git(&["-C", &dest_str, "checkout", "-q", commit])
            .await
            .map_err(|_| anyhow::anyhow!("commit {commit} not found for {repo}"))?;
    }
    Ok(())
}

/// Extract the working-tree diff, including newly created files. The runtime
/// writes sessions/checkpoints under `.deep-code/` inside the workspace —
/// exclude it so agent bookkeeping never leaks into the model patch.
async fn extract_git_diff(workspace: &Path) -> anyhow::Result<String> {
    let ws = workspace.to_string_lossy().into_owned();
    git(&["-C", &ws, "add", "-A", "--", ".", ":(exclude).deep-code"]).await?;
    let output = tokio::process::Command::new("git")
        .args(["-C", &ws, "diff", "--cached"])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("git diff --cached failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Current UTC time as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`), std-only.
fn utc_now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamp_shape() {
        let ts = utc_now_iso();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        // Sanity: we are past 2025 and before 2100.
        let year: i64 = ts[..4].parse().unwrap();
        assert!((2025..2100).contains(&year), "{ts}");
    }

    #[test]
    fn prompt_wraps_issue_with_task_framing() {
        #[derive(Debug)]
        struct Fake;
        impl BenchmarkInstance for Fake {
            fn instance_id(&self) -> &str {
                "x__x-1"
            }
            fn problem_statement(&self) -> &str {
                "Something is broken"
            }
            fn repo(&self) -> &str {
                "x/x"
            }
            fn base_commit(&self) -> &str {
                "abc"
            }
            fn hints(&self) -> Option<&str> {
                None
            }
        }
        let prompt = instance_prompt(&Fake);
        assert!(prompt.contains("git checkout of x/x"));
        assert!(prompt.contains("<issue>\nSomething is broken\n</issue>"));
        assert!(prompt.contains("Do NOT modify any test files"));
    }
}
