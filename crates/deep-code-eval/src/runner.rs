//! Evaluation runner: drives the agent against each benchmark instance and
//! produces official-format predictions (patches). Scoring is NOT done here —
//! a non-empty patch is not "resolved"; submit the predictions to the official
//! SWE-bench harness (sb-cli) for the real resolved rate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deep_code_agent::{
    AgentConfig, ApprovalDecision, LaunchedRuntime, Role, RuntimeEvent, RuntimeEventReceiver,
    TurnTelemetry, launch_runtime,
};
use tokio::sync::{Mutex as TokioMutex, Semaphore};
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
    /// Where to copy each instance's session transcript before its throwaway
    /// workspace is dropped. `None` discards them.
    pub transcripts_dir: Option<PathBuf>,
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
            transcripts_dir: None,
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
    /// Model requests this turn made — the agent loop's own step unit, one per
    /// API round.
    ///
    /// Counted off the session transcript rather than read from telemetry, and
    /// deliberately so: the abort paths (the step cap, a cancel, a timeout) emit
    /// no telemetry, and those are exactly the turns this column exists to
    /// explain. Reading the transcript instead means an abnormally-ended
    /// instance still reports a real number rather than a zero standing in for
    /// "unknown".
    #[serde(default)]
    pub api_rounds: u32,
    /// Tool calls across the turn's assistant messages. Not `api_rounds`: one
    /// round can carry several calls in parallel, so this is the count that
    /// shows how hard the turn actually worked.
    #[serde(default)]
    pub tool_calls: u32,
    /// The turn's request ceiling **as it applied to this instance**, so the
    /// column below answers about the cap that was actually in force rather than
    /// about a constant this crate also happens to know. `0` means the run was
    /// unlimited by configuration.
    #[serde(default)]
    pub max_turn_steps: u32,
    /// True when `api_rounds` reached [`Self::max_turn_steps`], i.e. the turn was
    /// cut by the cap rather than by the model deciding it was done.
    #[serde(default)]
    pub step_limit_hit: bool,
    /// Session cost in CNY (from turn telemetry; 0 if unavailable).
    pub cost_cny: f64,
    /// Effective model of the turn (e.g. deepseek-flash), if reported.
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
    /// `model_name_or_path` for the official predictions — identifies this
    /// submission by agent version and routing config.
    pub model_name: String,
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
    // `sandbox_available()` alone is not enough: Windows reports a backend
    // (the Job Object) as available while it confines neither writes nor
    // network, so the refusal that promises "run inside a container" never
    // fired there and eval ran model commands unconfined. Require the sandbox
    // to actually ENFORCE something — `is_enforced()` accepts a Partial Linux
    // kernel (writes still bounded) but rejects Windows' `None`.
    // Configuration can turn the sandbox off; eval must not accept that. The
    // host check below proves a backend exists, and this one proves the run is
    // actually using it — else a `sandbox_mode = "off"` in a config file would
    // silently turn every rollout into unconfined model commands on this
    // machine, which is the exact scenario the refusal exists for.
    anyhow::ensure!(
        config.agent_config.sandbox_mode.is_os(),
        "refusing to run eval with [sandbox] mode = \"off\": eval auto-approves model \
         commands on untrusted repositories, so the OS sandbox is the only boundary between \
         a generated command and this machine. Remove the setting (or set mode = \"os\") for \
         the rollout."
    );
    anyhow::ensure!(
        deep_code_agent::sandbox_available()
            && deep_code_agent::sandbox_enforcement().is_enforced(),
        "refusing to run eval without an enforcing OS sandbox: eval auto-approves \
         model commands on untrusted repos, and this machine's sandbox confines \
         nothing (no backend, or a Windows Job Object that restricts neither \
         writes nor network). Run inside a container, or on macOS/Linux with \
         sandbox support."
    );
    let started_at = utc_now_iso();
    let start = Instant::now();

    let instances: Vec<_> = bench_set.instances.clone();
    let semaphore = Arc::new(Semaphore::new(config.parallelism.max(1)));

    let mut handles = Vec::with_capacity(instances.len());
    for instance in instances {
        let permit = semaphore.clone().acquire_owned().await?;
        let config = config.clone();
        let instance_id = instance.instance_id().to_string();

        handles.push((
            instance_id,
            tokio::spawn(async move {
                let _permit = permit;
                run_single(&config, &instance).await
            }),
        ));
    }

    // Results come back through the join, not a shared sink, so an instance
    // whose task panicked or was aborted still lands in the report as an
    // error. It would otherwise vanish: official scoring counts a missing
    // instance as unresolved, and `total` derived from the collected results
    // would agree with itself — a short report costing real score with
    // nothing flagging the gap.
    let mut final_results = Vec::with_capacity(handles.len());
    for (instance_id, handle) in handles {
        match handle.await {
            Ok(result) => final_results.push(result),
            Err(join_error) => {
                eprintln!("  💥 {instance_id}: instance task did not finish ({join_error})");
                final_results.push(InstanceResult {
                    instance_id,
                    status: InstanceStatus::Error,
                    patch: String::new(),
                    duration_ms: 0,
                    api_rounds: 0,
                    tool_calls: 0,
                    max_turn_steps: 0,
                    step_limit_hit: false,
                    cost_cny: 0.0,
                    model: None,
                    route_source: None,
                    cascade_triggered: false,
                    error: Some(format!("instance task did not finish: {join_error}")),
                });
            }
        }
    }
    // Join order follows spawn order, but don't inherit the loader's ordering
    // as an invariant: sort so reports and predictions stay diffable.
    final_results.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let model_name = submission_name(&config.agent_config.model);

    let count =
        |status: InstanceStatus| final_results.iter().filter(|r| r.status == status).count();
    Ok(BenchReport {
        bench: config.bench,
        subset: config.subset,
        split: config.split,
        model_name,
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

/// `model_name_or_path` for the official predictions: agent version plus the
/// routing config. A bare "deep-code" would make two runs from different
/// versions or model pins indistinguishable on a leaderboard. An unset model
/// reports the routing sentinel (`auto`) rather than inventing a model id —
/// which models `auto` actually picked is recorded per instance in the report.
fn submission_name(pinned_model: &str) -> String {
    let pinned = pinned_model.trim();
    format!(
        "deep-code-{}-{}",
        env!("CARGO_PKG_VERSION"),
        if pinned.is_empty() { "auto" } else { pinned }
    )
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
        api_rounds: 0,
        tool_calls: 0,
        max_turn_steps: 0,
        step_limit_hit: false,
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
    // Bound the checkout too, not just the turn: a stalled clone/fetch (git has
    // no timeout of its own) would otherwise hold this parallelism slot forever
    // and write nothing until every other instance finished.
    match timeout(
        config.instance_timeout,
        checkout_repo(instance.repo(), instance.base_commit(), workspace.path()),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return error_result(format!("checkout failed: {e}"), &start),
        Err(_) => return error_result("checkout timed out".to_string(), &start),
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
    // Read the accumulated session spend BEFORE shutting the runtime down: on a
    // timeout the turn carries no telemetry (`TurnOutcome::default()`), so cost
    // fell to 0 for exactly the instances that usually cost the most, and the
    // report's total ran systematically low. `session_spend` holds the real
    // accumulated cost however the turn ended.
    let session_spend = launched.handle.session_spend().await;
    // The turn's shape, read here for the same reason the spend is: this is the
    // last moment the transcript is reachable, and the instances that need
    // explaining are the ones that ended abnormally (hence no telemetry).
    // Rounds and calls are counted, never guessed.
    let (api_rounds, tool_calls) = {
        let messages = launched.handle.session_messages().await;
        let mut rounds = 0u32;
        let mut calls = 0u32;
        for message in &messages {
            if message.role == Role::Assistant {
                rounds = rounds.saturating_add(1);
                calls = calls
                    .saturating_add(u32::try_from(message.tool_calls.len()).unwrap_or(u32::MAX));
            }
        }
        (rounds, calls)
    };
    let max_turn_steps = config.agent_config.turn_steps;
    let step_limit_hit = max_turn_steps > 0 && api_rounds >= max_turn_steps;
    // Fully stop the runtime before extracting the diff.
    launched.shutdown().await;

    // Preserve the transcript while the workspace still exists. The report
    // records THAT an instance struggled (cascade, error, timeout), never what
    // the agent actually did — and the workspace is a temp dir that takes the
    // session with it. Without this, diagnosing a low score means re-running
    // the whole split. Copied before patch extraction so even an extraction
    // failure keeps its evidence.
    if let Some(dir) = config.transcripts_dir.as_deref()
        && let Err(error) = save_transcript(workspace.path(), dir, &instance_id).await
    {
        eprintln!("warning: transcript not saved for {instance_id}: {error}");
    }

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
        // No telemetry (timeout/cancel/error): fall back to the accumulated
        // session spend so a timed-out instance still reports its real cost.
        None => (session_spend.cost.cny, None, None, false),
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
        api_rounds,
        tool_calls,
        max_turn_steps,
        step_limit_hit,
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
            RuntimeEvent::ApprovalRequired { ref request, .. } => {
                // The benchmark approves everything so a run is unattended —
                // but not a request to WIDEN the write boundary. This harness
                // exists to run a model over untrusted checkouts, and a
                // granted root is a real sandbox write grant, so a prompt
                // injected into a benchmark repo could ask for the
                // evaluator's own `~/.cargo` and get a yes with nobody
                // present. Every other unattended channel (headless `-p`,
                // serve, sub-agents) already refuses this one; this was the
                // only place that said yes.
                //
                // The denial carries the real reason for the same reason the
                // sub-agent gate does: nobody saw this prompt, so the stock
                // "User declined the write-root request" would teach the model
                // a refusal that never happened — here, in a rollout whose
                // whole output is what the model did next.
                let (decision, denial_note) =
                    if request.tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL {
                        (
                            ApprovalDecision::Denied,
                            Some(deep_code_agent::unattended_denial_note(request)),
                        )
                    } else {
                        (ApprovalDecision::Approved, None)
                    };
                receiver = launched
                    .handle
                    .submit_approval_with_denial_note(decision, denial_note)
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

/// Copy one instance's session transcripts out of its throwaway workspace,
/// into `<dest_root>/<instance_id>/`. Only `sessions/` — `checkpoints/` next
/// to it are full workspace snapshots, gigabytes across a split.
async fn save_transcript(
    workspace: &Path,
    dest_root: &Path,
    instance_id: &str,
) -> anyhow::Result<()> {
    let sessions = workspace.join(".deep-code").join("sessions");
    if !sessions.is_dir() {
        return Ok(()); // persistence was unavailable for this instance
    }
    let dest = dest_root.join(instance_id);
    tokio::fs::create_dir_all(&dest).await?;
    let mut entries = tokio::fs::read_dir(&sessions).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // Only real files. `DirEntry::file_type` does NOT follow symlinks, so a
        // `sessions/x.json` symlink the agent planted (an ordinary workspace
        // write) is skipped rather than having `tokio::fs::copy` — which DOES
        // follow symlinks — copy its target (e.g. ~/.deep-code/config.toml, the
        // API key) out into eval-out, from this unsandboxed harness.
        if !entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "json")
            && let Some(name) = path.file_name()
        {
            tokio::fs::copy(&path, dest.join(name)).await?;
        }
    }
    Ok(())
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

/// How much of a failed git command's stderr to carry into the error.
const GIT_ERROR_TAIL: usize = 400;

async fn git(args: &[&str]) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        // Keep the reason. A bare "git clone failed" reads identically whether
        // the network dropped, the disk filled, or the commit is gone — and
        // cloning is the most failure-prone step of a multi-hour rollout, on
        // the machine least likely to be watched while it runs.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let reason = if trimmed.is_empty() {
            "(no stderr)".to_string()
        } else {
            // Tail-bounded and char-safe (paths and messages may be non-ASCII).
            let chars: Vec<char> = trimmed.chars().collect();
            chars[chars.len().saturating_sub(GIT_ERROR_TAIL)..]
                .iter()
                .collect()
        };
        anyhow::bail!("git {} failed: {reason}", args.join(" "));
    }
    Ok(())
}

/// One lock per repo id, so parallel instances of the same repo serialize their
/// use of the shared bare cache instead of racing `if !cache.exists() { clone }`
/// (three concurrent `clone --bare` into one path fail with exit 128, and an
/// instance that sees the dir mid-clone gets an empty repo and a failed
/// checkout). Cross-repo checkouts still run in parallel.
fn repo_lock_for(repo: &str) -> Arc<TokioMutex<()>> {
    static LOCKS: LazyLock<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>> =
        LazyLock::new(|| StdMutex::new(HashMap::new()));
    LOCKS
        .lock()
        .expect("repo-lock map poisoned")
        .entry(repo.to_string())
        .or_default()
        .clone()
}

/// Check out `repo` at `commit` into `dest`, going through the bare cache.
async fn checkout_repo(repo: &str, commit: &str, dest: &Path) -> anyhow::Result<()> {
    let cache = cache_dir().join(repo.replace('/', "__"));
    let cache_str = cache.to_string_lossy().into_owned();
    let dest_str = dest.to_string_lossy().into_owned();
    let url = format!("https://github.com/{repo}.git");

    // The whole checkout is serialized per repo: the `clone --shared` workdir
    // borrows objects from the cache, so a concurrent `fetch` into that cache
    // (the retry path below) could pull the alternates out from under it. The
    // agent turn dominates an instance's runtime, so serializing checkouts of
    // the same repo costs little; different repos still overlap.
    let lock = repo_lock_for(repo);
    let _guard = lock.lock().await;

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
    // The agent just ran over an UNTRUSTED benchmark repo and may have written
    // .git/config, hooks, core.fsmonitor, or a .gitattributes ext-diff/textconv
    // driver to run code inside these unsandboxed git commands. Override the
    // execution surfaces we can: no system config, hooks disabled, fsmonitor
    // off, and the diff drivers suppressed. Residual: an in-tree .gitattributes
    // clean filter still runs on `git add` (there is no per-invocation switch to
    // disable in-tree filters); the dataset repos are the official SWE-bench set.
    let base: &[&str] = &[
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-C",
        &ws,
    ];
    let run = |extra: &[&str]| {
        let args: Vec<String> = base
            .iter()
            .chain(extra.iter())
            .map(|s| (*s).to_string())
            .collect();
        async move {
            tokio::process::Command::new("git")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(&args)
                .output()
                .await
        }
    };
    let add = run(&["add", "-A", "--", ".", ":(exclude).deep-code"]).await?;
    if !add.status.success() {
        anyhow::bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }
    let output = run(&["diff", "--cached", "--no-ext-diff", "--no-textconv"]).await?;
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

    #[tokio::test]
    async fn transcript_copy_takes_sessions_and_leaves_checkpoints() {
        let workspace = tempfile::tempdir().unwrap();
        let agent_dir = workspace.path().join(".deep-code");
        tokio::fs::create_dir_all(agent_dir.join("sessions"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(agent_dir.join("checkpoints").join("ckpt_1"))
            .await
            .unwrap();
        tokio::fs::write(agent_dir.join("sessions/session_1.json"), b"{}")
            .await
            .unwrap();
        tokio::fs::write(agent_dir.join("sessions/notes.txt"), b"x")
            .await
            .unwrap();
        tokio::fs::write(agent_dir.join("checkpoints/ckpt_1/big.bin"), b"snapshot")
            .await
            .unwrap();

        let out = tempfile::tempdir().unwrap();
        save_transcript(workspace.path(), out.path(), "repo__repo-1")
            .await
            .unwrap();

        let dest = out.path().join("repo__repo-1");
        assert!(dest.join("session_1.json").is_file());
        assert!(
            !dest.join("notes.txt").exists(),
            "only session json travels"
        );
        // The load-bearing one: checkpoints are full workspace snapshots, so
        // copying them would mean gigabytes across a 300-instance split.
        assert!(!dest.join("ckpt_1").exists());
        assert!(!dest.join("big.bin").exists());
    }

    /// A `.json` entry in sessions/ that is actually a SYMLINK (an ordinary
    /// workspace write the agent can make) must not be followed: the unsandboxed
    /// harness would otherwise copy the link's target — e.g. a credential file —
    /// into eval-out.
    #[cfg(unix)]
    #[tokio::test]
    async fn transcript_copy_skips_symlinked_session_files() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = workspace.path().join(".deep-code").join("sessions");
        tokio::fs::create_dir_all(&sessions).await.unwrap();

        // A real session file, and a secret sitting outside the workspace.
        tokio::fs::write(sessions.join("real.json"), b"{}")
            .await
            .unwrap();
        let secret = tempfile::tempdir().unwrap();
        tokio::fs::write(secret.path().join("id_rsa"), b"PRIVATE KEY")
            .await
            .unwrap();
        // A .json-named symlink pointing at the secret.
        std::os::unix::fs::symlink(secret.path().join("id_rsa"), sessions.join("stolen.json"))
            .unwrap();

        let out = tempfile::tempdir().unwrap();
        save_transcript(workspace.path(), out.path(), "repo__repo-1")
            .await
            .unwrap();

        let dest = out.path().join("repo__repo-1");
        assert!(
            dest.join("real.json").is_file(),
            "real sessions still travel"
        );
        assert!(
            !dest.join("stolen.json").exists(),
            "a symlinked session file must not be followed and copied out"
        );
    }

    #[tokio::test]
    async fn transcript_copy_tolerates_a_missing_sessions_dir() {
        // Persistence can be unavailable for an instance (the launch reports it
        // as a warning); that must not fail the instance or the rollout.
        let workspace = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(
            save_transcript(workspace.path(), out.path(), "x__x-1")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn git_failure_carries_the_reason() {
        let error = git(&["-C", "/deep-code-no-such-path", "status"])
            .await
            .expect_err("git must fail on a nonexistent directory")
            .to_string();
        assert!(error.contains("git -C"), "{error}");
        // The whole point of capturing stderr: locale-independent check that a
        // reason arrived instead of the placeholder.
        assert!(!error.contains("(no stderr)"), "{error}");
    }

    #[test]
    fn submission_name_carries_version_and_routing() {
        let auto = submission_name("auto");
        assert!(auto.starts_with("deep-code-"), "{auto}");
        assert!(auto.contains(env!("CARGO_PKG_VERSION")), "{auto}");
        assert!(auto.ends_with("-auto"), "{auto}");
        // An unset model reports the sentinel, never a bare agent name.
        assert_eq!(submission_name("   "), auto);
        // A pin is carried verbatim so two runs stay distinguishable.
        assert!(submission_name("deepseek-v4-pro").ends_with("-deepseek-v4-pro"));
    }

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
