//! Exercise the real scheduler/checkpoint path with deterministic local jobs.
//! No repository checkout, network access or model client is involved.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tokio::sync::Notify;

use super::*;

#[derive(Debug, Clone)]
struct Fixture(&'static str);

impl BenchmarkInstance for Fixture {
    fn instance_id(&self) -> &str {
        self.0
    }

    fn problem_statement(&self) -> &str {
        "Fix the fixture regression"
    }

    fn repo(&self) -> &str {
        "fixture/repository"
    }

    fn base_commit(&self) -> &str {
        "abc123"
    }
}

fn benchmark(ids: &[&'static str]) -> BenchmarkSet<Fixture> {
    BenchmarkSet {
        instances: ids.iter().map(|id| Fixture(id)).collect(),
    }
}

fn configuration(directory: &Path) -> EvalConfig {
    EvalConfig {
        agent_config: AgentConfig::builtin(),
        output_dir: Some(directory.to_path_buf()),
        ..EvalConfig::default()
    }
}

fn result(id: &str, status: InstanceStatus) -> InstanceResult {
    let patch = if status == InstanceStatus::PatchProduced {
        format!("patch for {id}")
    } else {
        String::new()
    };
    let error = matches!(status, InstanceStatus::Error | InstanceStatus::Timeout)
        .then(|| format!("fixture {status:?}"));
    InstanceResult {
        instance_id: id.into(),
        status,
        patch,
        duration_ms: 10,
        api_rounds: 1,
        tool_calls: 1,
        max_turn_steps: 5,
        step_limit_hit: false,
        cost_cny: 0.125,
        model: Some("fixture-model".into()),
        route_source: Some("fixture".into()),
        cascade_triggered: false,
        error,
    }
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn saved_ids(directory: &Path) -> Vec<String> {
    read_json(directory.join("checkpoint.json"))["state"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["instance_id"].as_str().unwrap().to_string())
        .collect()
}

fn export_ids(directory: &Path) -> Vec<String> {
    read_json(directory.join("predictions.json"))
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["instance_id"].as_str().unwrap().to_string())
        .collect()
}

async fn notified(signal: &Notify, why: &str) {
    timeout(Duration::from_secs(5), signal.notified())
        .await
        .expect(why);
}

async fn wait_for_saved(directory: &Path, expected: &[&str]) {
    timeout(Duration::from_secs(5), async {
        loop {
            let path = directory.join("checkpoint.json");
            if path.is_file() && saved_ids(directory) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("completed jobs must be persisted without waiting for other jobs");
}

#[tokio::test]
async fn interrupted_rollout_keeps_exports_and_resumes_only_unsaved_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let config = configuration(directory.path());
    let bench = benchmark(&["first", "unfinished"]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(Notify::new());
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_release = Arc::new(Notify::new());
    let cleanup_done = Arc::new(Notify::new());

    let invocation = {
        let config = config.clone();
        let bench = bench.clone();
        let calls = calls.clone();
        let started = started.clone();
        let cleanup_started = cleanup_started.clone();
        let cleanup_release = cleanup_release.clone();
        let cleanup_done = cleanup_done.clone();
        tokio::spawn(async move {
            run_bench_with(config, &bench, move |_, instance, cancel| {
                let calls = calls.clone();
                let started = started.clone();
                let cleanup_started = cleanup_started.clone();
                let cleanup_release = cleanup_release.clone();
                let cleanup_done = cleanup_done.clone();
                async move {
                    calls.lock().unwrap().push(instance.0);
                    if instance.0 == "unfinished" {
                        started.notify_one();
                        cancel.cancelled().await;
                        cleanup_started.notify_one();
                        cleanup_release.notified().await;
                        cleanup_done.notify_one();
                    }
                    result(instance.0, InstanceStatus::PatchProduced)
                }
            })
            .await
        })
    };

    // Serial scheduling starts this job only after the first job is durable.
    notified(&started, "the second job must start").await;
    assert_eq!(saved_ids(directory.path()), ["first"]);
    assert_eq!(export_ids(directory.path()), ["first"]);
    let partial = read_json(directory.path().join("report.json"));
    assert_eq!(partial["total"], 2);
    assert_eq!(partial["results"].as_array().unwrap().len(), 1);

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    notified(
        &cleanup_started,
        "dropping the coordinator must cancel in-flight jobs",
    )
    .await;

    let mut resumed_config = config;
    resumed_config.resume = true;
    let premature_calls = Arc::new(AtomicUsize::new(0));
    let counter = premature_calls.clone();
    let error = run_bench_with(resumed_config.clone(), &bench, move |_, instance, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move { result(instance.0, InstanceStatus::PatchProduced) }
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already in use"), "{error:#}");
    assert_eq!(premature_calls.load(Ordering::SeqCst), 0);

    cleanup_release.notify_one();
    notified(&cleanup_done, "cancelled jobs must finish their cleanup").await;
    let report = run_bench_with(resumed_config, &bench, {
        let calls = calls.clone();
        move |_, instance, _| {
            calls.lock().unwrap().push(instance.0);
            async move { result(instance.0, InstanceStatus::PatchProduced) }
        }
    })
    .await
    .unwrap();

    assert_eq!(
        *calls.lock().unwrap(),
        ["first", "unfinished", "unfinished"]
    );
    assert_eq!(report.results.len(), 2);
    assert_eq!(report.patches_produced, 2);
    assert_eq!(report.total_cost_cny, 0.25);
    assert_eq!(saved_ids(directory.path()), ["first", "unfinished"]);
}

#[tokio::test]
async fn parallel_fast_job_is_saved_before_an_earlier_slow_job_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = configuration(directory.path());
    config.parallelism = 2;
    let bench = benchmark(&["slow", "fast"]);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let invocation = {
        let started = started.clone();
        let release = release.clone();
        tokio::spawn(async move {
            run_bench_with(config, &bench, move |_, instance, cancel| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    if instance.0 == "slow" {
                        started.notify_one();
                        tokio::select! {
                            _ = release.notified() => {},
                            _ = cancel.cancelled() => {},
                        }
                    }
                    result(instance.0, InstanceStatus::PatchProduced)
                }
            })
            .await
        })
    };

    notified(&started, "the first job must start and remain blocked").await;
    wait_for_saved(directory.path(), &["fast"]).await;
    // The checkpoint commits before exports; wait for that derived view too.
    timeout(Duration::from_secs(5), async {
        while export_ids(directory.path()) != ["fast"] {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fast job exports must be visible while the slow job is still running");
    assert!(!invocation.is_finished());
    release.notify_one();
    let report = timeout(Duration::from_secs(5), invocation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ids: Vec<_> = report
        .results
        .iter()
        .map(|entry| entry.instance_id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["fast", "slow"],
        "completion order must not scramble reports"
    );
}

#[tokio::test]
async fn all_saved_terminal_statuses_skip_execution_and_rebuild_missing_exports() {
    let directory = tempfile::tempdir().unwrap();
    let config = configuration(directory.path());
    let bench = benchmark(&["patch", "empty", "timeout", "error"]);
    let first = run_bench_with(config.clone(), &bench, |_, instance, _| async move {
        let status = match instance.0 {
            "patch" => InstanceStatus::PatchProduced,
            "empty" => InstanceStatus::EmptyPatch,
            "timeout" => InstanceStatus::Timeout,
            "error" => InstanceStatus::Error,
            _ => unreachable!(),
        };
        result(instance.0, status)
    })
    .await
    .unwrap();
    std::fs::remove_file(directory.path().join("report.json")).unwrap();
    std::fs::remove_file(directory.path().join("predictions.json")).unwrap();

    let mut resumed = config;
    resumed.resume = true;
    resumed.parallelism = 4;
    resumed.agent_config.api_key = Some("a-different-key-not-persisted".into());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let report = run_bench_with(resumed, &bench, move |_, instance, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move { result(instance.0, InstanceStatus::PatchProduced) }
    })
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(report.total, 4);
    assert_eq!(report.results.len(), 4);
    assert_eq!(report.patches_produced, 1);
    assert_eq!(report.empty_patches, 1);
    assert_eq!(report.timeouts, 1);
    assert_eq!(report.errors, 1);
    assert_eq!(report.total_cost_cny, first.total_cost_cny);
    assert_eq!(report.started_at, first.started_at);
    assert_eq!(
        export_ids(directory.path()),
        ["empty", "error", "patch", "timeout"]
    );
    assert_eq!(read_json(directory.path().join("report.json"))["errors"], 1);
    assert!(
        !std::fs::read_to_string(directory.path().join("checkpoint.json"))
            .unwrap()
            .contains("a-different-key-not-persisted")
    );
}

#[tokio::test]
async fn export_failure_preserves_current_result_and_stops_new_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let config = configuration(directory.path());
    let bench = benchmark(&["saved-before-export-failed", "not-started"]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let error = run_bench_with(config.clone(), &bench, move |config, instance, _| {
        log.lock().unwrap().push(instance.0);
        async move {
            let report_path = config.output_dir.unwrap().join("report.json");
            std::fs::remove_file(&report_path).unwrap();
            std::fs::create_dir(&report_path).unwrap();
            result(instance.0, InstanceStatus::PatchProduced)
        }
    })
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("failed to save eval progress"),
        "{error:#}"
    );
    assert_eq!(*calls.lock().unwrap(), ["saved-before-export-failed"]);
    assert_eq!(saved_ids(directory.path()), ["saved-before-export-failed"]);
    std::fs::remove_dir(directory.path().join("report.json")).unwrap();

    let mut resumed = config;
    resumed.resume = true;
    let log = calls.clone();
    let report = run_bench_with(resumed, &bench, move |_, instance, _| {
        log.lock().unwrap().push(instance.0);
        async move { result(instance.0, InstanceStatus::PatchProduced) }
    })
    .await
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        ["saved-before-export-failed", "not-started"]
    );
    assert_eq!(report.results.len(), 2);
    assert_eq!(
        export_ids(directory.path()),
        ["not-started", "saved-before-export-failed"]
    );
}

#[tokio::test]
async fn panicked_task_is_persisted_as_an_error_without_losing_an_instance() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = configuration(directory.path());
    config.parallelism = 2;
    let bench = benchmark(&["panic", "normal"]);
    let report = timeout(
        Duration::from_secs(5),
        run_bench_with(config, &bench, |_, instance, _| async move {
            assert_ne!(instance.0, "panic", "deliberate fixture task panic");
            result(instance.0, InstanceStatus::PatchProduced)
        }),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(report.total, 2);
    assert_eq!(report.results.len(), 2);
    assert_eq!(report.errors, 1);
    assert_eq!(report.patches_produced, 1);
    let panicked = report
        .results
        .iter()
        .find(|entry| entry.instance_id == "panic")
        .unwrap();
    assert_eq!(panicked.status, InstanceStatus::Error);
    assert!(
        panicked
            .error
            .as_deref()
            .unwrap()
            .contains("instance task did not finish")
    );
    assert_eq!(saved_ids(directory.path()), ["normal", "panic"]);
}

#[tokio::test]
async fn invalid_resume_checkpoint_never_starts_an_executor() {
    for invalid in ["corrupt-json", "changed-config", "unknown-saved-id"] {
        let directory = tempfile::tempdir().unwrap();
        let config = configuration(directory.path());
        let bench = benchmark(&["selected"]);
        run_bench_with(config.clone(), &bench, |_, instance, _| async move {
            result(instance.0, InstanceStatus::PatchProduced)
        })
        .await
        .unwrap();
        let checkpoint_path = directory.path().join("checkpoint.json");
        let mut resumed = config;
        resumed.resume = true;
        match invalid {
            "corrupt-json" => std::fs::write(&checkpoint_path, "{incomplete").unwrap(),
            "changed-config" => resumed.agent_config.turn_steps += 1,
            "unknown-saved-id" => {
                let mut checkpoint = read_json(&checkpoint_path);
                checkpoint["state"]["results"][0]["instance_id"] = "outside-selection".into();
                std::fs::write(&checkpoint_path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let error = run_bench_with(resumed, &bench, move |_, instance, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            async move { result(instance.0, InstanceStatus::PatchProduced) }
        })
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        match invalid {
            "corrupt-json" => assert!(message.contains("invalid or incomplete"), "{message}"),
            "changed-config" => {
                assert!(message.contains("differs from the saved run"), "{message}")
            }
            "unknown-saved-id" => assert!(message.contains("unselected instance"), "{message}"),
            _ => unreachable!(),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{invalid}");
    }
}
