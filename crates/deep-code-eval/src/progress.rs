//! Durable rollout progress. The checkpoint is authoritative; exports can be
//! rebuilt from it after an interruption between the two export writes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runner::{BenchReport, InstanceResult};

const SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_FILE: &str = "checkpoint.json";
const REPORT_FILE: &str = "report.json";
const PREDICTIONS_FILE: &str = "predictions.json";

/// Only terminal instance results belong here. An interrupted in-flight
/// instance has no result yet and is eligible to run on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProgressState {
    pub started_at: String,
    /// Active elapsed time across invocations, excluding time between runs.
    pub duration_ms: u64,
    pub results: Vec<InstanceResult>,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    schema_version: u32,
    signature: Value,
    state: ProgressState,
}

/// A single writer owns the output directory for its whole rollout. The lock
/// file intentionally stays on disk: deleting it while another process opens
/// it would let two processes lock different inodes for the same directory.
pub(crate) struct ProgressStore {
    directory: PathBuf,
    signature: Value,
    state: ProgressState,
    base_duration_ms: u64,
    _lock: File,
}

impl ProgressStore {
    pub(crate) fn open(
        directory: &Path,
        resume: bool,
        signature: &Value,
        started_at: &str,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(directory).context("failed to create eval output directory")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(".eval.lock"))
            .context("failed to open eval output lock")?;
        lock.try_lock().map_err(|error| {
            anyhow::anyhow!("eval output directory is already in use or cannot be locked: {error}")
        })?;

        let checkpoint_path = directory.join(CHECKPOINT_FILE);
        let state = if resume {
            let file = File::open(&checkpoint_path).context(
                "cannot resume: checkpoint.json is missing or unreadable; use the original --out directory",
            )?;
            let checkpoint: Checkpoint = serde_json::from_reader(file)
                .context("cannot resume: checkpoint.json is invalid or incomplete")?;
            ensure!(
                checkpoint.schema_version == SCHEMA_VERSION,
                "cannot resume: checkpoint schema version is not supported"
            );
            ensure!(
                checkpoint.signature == *signature,
                "cannot resume: benchmark data or rollout configuration differs from the saved run"
            );
            let mut state = checkpoint.state;
            state
                .results
                .sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
            ensure!(
                !state
                    .results
                    .windows(2)
                    .any(|pair| pair[0].instance_id == pair[1].instance_id),
                "cannot resume: checkpoint contains duplicate instance results"
            );
            state
        } else {
            for name in [CHECKPOINT_FILE, REPORT_FILE, PREDICTIONS_FILE] {
                ensure!(
                    !entry_exists(&directory.join(name))?,
                    "eval output already contains {name}; use --resume to continue or choose a different --out directory"
                );
            }
            ProgressState {
                started_at: started_at.to_string(),
                duration_ms: 0,
                results: Vec::new(),
            }
        };
        let store = Self {
            directory: directory.to_path_buf(),
            signature: signature.clone(),
            base_duration_ms: state.duration_ms,
            state,
            _lock: lock,
        };
        if !resume {
            store.write_checkpoint(&store.state)?;
        }
        Ok(store)
    }

    pub(crate) fn state(&self) -> &ProgressState {
        &self.state
    }

    /// `elapsed_ms` is cumulative active time since this invocation started,
    /// not the duration of this one instance and not an amount to add twice.
    pub(crate) fn record(&mut self, result: InstanceResult, elapsed_ms: u64) -> anyhow::Result<()> {
        ensure!(
            !self
                .state
                .results
                .iter()
                .any(|saved| saved.instance_id == result.instance_id),
            "refusing to save duplicate eval result for {}",
            result.instance_id
        );
        let mut next = self.state.clone();
        next.results.push(result);
        next.results
            .sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        next.duration_ms = self.base_duration_ms.saturating_add(elapsed_ms);
        // Do not expose an in-memory result as durable until its replacement
        // checkpoint has been written and synchronized successfully.
        self.write_checkpoint(&next)?;
        self.state = next;
        Ok(())
    }

    /// Each file is atomically replaced. The checkpoint commits first, so a
    /// crash between these two derived writes is repaired by the next resume.
    pub(crate) fn write_exports(&self, report: &BenchReport) -> anyhow::Result<()> {
        write_json(&self.directory.join(REPORT_FILE), report)?;
        let predictions = crate::report::to_predictions_json(report)?;
        atomic_write(&self.directory.join(PREDICTIONS_FILE), |file| {
            file.write_all(predictions.as_bytes())?;
            file.write_all(b"\n")?;
            Ok(())
        })
    }

    fn write_checkpoint(&self, state: &ProgressState) -> anyhow::Result<()> {
        write_json(
            &self.directory.join(CHECKPOINT_FILE),
            &Checkpoint {
                schema_version: SCHEMA_VERSION,
                signature: self.signature.clone(),
                state: state.clone(),
            },
        )
    }
}

fn entry_exists(path: &Path) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("failed to inspect existing eval output"),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    atomic_write(path, |file| {
        serde_json::to_writer_pretty(&mut *file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    })
}

fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut File) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("eval output has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("failed to create temporary eval output")?;
    write(temporary.as_file_mut()).context("failed to write temporary eval output")?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .context("failed to replace eval output")?;
    // Persist the directory entry as well as the file contents. Windows does
    // not support opening directories through std::fs::File this way.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::InstanceStatus;

    fn signature() -> Value {
        serde_json::json!({"dataset": "test-fixture", "model": "mock"})
    }

    fn result(id: &str) -> InstanceResult {
        InstanceResult {
            instance_id: id.to_string(),
            status: InstanceStatus::PatchProduced,
            patch: format!("patch for {id}"),
            duration_ms: 10,
            api_rounds: 1,
            tool_calls: 1,
            max_turn_steps: 5,
            step_limit_hit: false,
            cost_cny: 0.01,
            model: Some("mock".to_string()),
            route_source: None,
            cascade_triggered: false,
            error: None,
        }
    }

    fn open(directory: &Path, resume: bool) -> anyhow::Result<ProgressStore> {
        ProgressStore::open(directory, resume, &signature(), "2026-11-13T01:02:03Z")
    }

    fn report(state: &ProgressState) -> BenchReport {
        BenchReport {
            bench: "swe-bench".into(),
            subset: "lite".into(),
            split: "dev".into(),
            model_name: "mock".into(),
            started_at: state.started_at.clone(),
            duration_ms: state.duration_ms,
            total: state.results.len(),
            patches_produced: state.results.len(),
            empty_patches: 0,
            timeouts: 0,
            errors: 0,
            total_cost_cny: state.results.iter().map(|r| r.cost_cny).sum(),
            results: state.results.clone(),
        }
    }

    #[test]
    fn each_record_is_durable_sorted_and_resumable() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("unrelated.txt"), "keep").unwrap();
        let mut store = open(directory.path(), false).unwrap();
        assert!(directory.path().join(CHECKPOINT_FILE).is_file());
        store.record(result("z"), 10).unwrap();
        store.record(result("a"), 25).unwrap();
        assert_eq!(store.state().duration_ms, 25);
        drop(store);

        let mut resumed = open(directory.path(), true).unwrap();
        assert_eq!(resumed.state().started_at, "2026-11-13T01:02:03Z");
        assert_eq!(resumed.state().results[0].instance_id, "a");
        assert_eq!(resumed.state().results[1].instance_id, "z");
        resumed.record(result("b"), 7).unwrap();
        resumed.record(result("c"), 12).unwrap();
        assert_eq!(
            resumed.state().duration_ms,
            37,
            "elapsed time is not added twice"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("unrelated.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn duplicate_record_does_not_change_durable_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(directory.path(), false).unwrap();
        store.record(result("a"), 10).unwrap();
        let before = std::fs::read(directory.path().join(CHECKPOINT_FILE)).unwrap();
        assert!(store.record(result("a"), 20).is_err());
        assert_eq!(store.state().results.len(), 1);
        assert_eq!(store.state().duration_ms, 10);
        assert_eq!(
            std::fs::read(directory.path().join(CHECKPOINT_FILE)).unwrap(),
            before
        );
    }

    #[test]
    fn concurrent_writer_is_refused_and_drop_releases_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let first = open(directory.path(), false).unwrap();
        let error = open(directory.path(), true)
            .err()
            .expect("lock must be exclusive");
        assert!(error.to_string().contains("already in use"), "{error:#}");
        drop(first);
        assert!(open(directory.path(), true).is_ok());
    }

    #[tokio::test]
    async fn output_lock_is_enforced_across_processes() {
        const CHILD_DIRECTORY: &str = "DEEPCODE_TEST_EVAL_LOCK_DIRECTORY";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            let error = open(Path::new(&directory), true)
                .err()
                .expect("another process owns this output directory");
            assert!(error.to_string().contains("already in use"), "{error:#}");
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let _store = open(directory.path(), false).unwrap();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "progress::tests::output_lock_is_enforced_across_processes",
                    "--nocapture",
                ])
                .env(CHILD_DIRECTORY, directory.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("the competing writer must fail promptly")
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn fresh_run_refuses_to_overwrite_any_managed_artifact() {
        for filename in [CHECKPOINT_FILE, REPORT_FILE, PREDICTIONS_FILE] {
            let directory = tempfile::tempdir().unwrap();
            let artifact = directory.path().join(filename);
            std::fs::write(&artifact, "old output").unwrap();
            let error = open(directory.path(), false).err().unwrap();
            assert!(error.to_string().contains("already contains"), "{error:#}");
            assert_eq!(std::fs::read_to_string(artifact).unwrap(), "old output");
        }
    }

    #[test]
    fn resume_refuses_missing_corrupted_or_mismatched_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        assert!(open(directory.path(), true).is_err());
        std::fs::write(directory.path().join(CHECKPOINT_FILE), "{partial").unwrap();
        let error = open(directory.path(), true).err().unwrap();
        assert!(
            error.to_string().contains("invalid or incomplete"),
            "{error:#}"
        );
        std::fs::remove_file(directory.path().join(CHECKPOINT_FILE)).unwrap();
        drop(open(directory.path(), false).unwrap());
        let error = ProgressStore::open(
            directory.path(),
            true,
            &serde_json::json!({"different": true}),
            "ignored",
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("differs"), "{error:#}");
    }

    #[test]
    fn resume_rejects_unsupported_schema_and_duplicate_saved_results() {
        let directory = tempfile::tempdir().unwrap();
        let store = open(directory.path(), false).unwrap();
        let mut checkpoint = Checkpoint {
            schema_version: SCHEMA_VERSION + 1,
            signature: signature(),
            state: store.state().clone(),
        };
        drop(store);
        write_json(&directory.path().join(CHECKPOINT_FILE), &checkpoint).unwrap();
        let error = open(directory.path(), true).err().unwrap();
        assert!(error.to_string().contains("schema version"), "{error:#}");
        checkpoint.schema_version = SCHEMA_VERSION;
        checkpoint.state.results = vec![result("a"), result("a")];
        write_json(&directory.path().join(CHECKPOINT_FILE), &checkpoint).unwrap();
        let error = open(directory.path(), true).err().unwrap();
        assert!(error.to_string().contains("duplicate"), "{error:#}");
    }

    #[test]
    fn interrupted_write_preserves_the_previous_complete_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(directory.path(), false).unwrap();
        store.record(result("a"), 10).unwrap();
        let path = directory.path().join(CHECKPOINT_FILE);
        let before = std::fs::read(&path).unwrap();
        let error = atomic_write(&path, |file| {
            file.write_all(b"{interrupted")?;
            anyhow::bail!("simulated disk write failure");
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("simulated disk write failure"));
        assert_eq!(std::fs::read(path).unwrap(), before);
        drop(store);
        assert_eq!(
            open(directory.path(), true).unwrap().state().results.len(),
            1
        );
    }

    #[test]
    fn exports_include_every_checkpointed_result_and_can_be_rebuilt() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(directory.path(), false).unwrap();
        store.record(result("b"), 10).unwrap();
        store.record(result("a"), 20).unwrap();
        store.write_exports(&report(store.state())).unwrap();
        let predictions: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join(PREDICTIONS_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(predictions.as_array().unwrap().len(), 2);
        assert_eq!(predictions[0]["instance_id"], "a");
        assert_eq!(predictions[1]["model_patch"], "patch for b");
        std::fs::write(directory.path().join(REPORT_FILE), "interrupted old export").unwrap();
        std::fs::remove_file(directory.path().join(PREDICTIONS_FILE)).unwrap();
        drop(store);
        let resumed = open(directory.path(), true).unwrap();
        resumed.write_exports(&report(resumed.state())).unwrap();
        let restored: BenchReport =
            serde_json::from_slice(&std::fs::read(directory.path().join(REPORT_FILE)).unwrap())
                .unwrap();
        assert_eq!(restored.total, 2);
        assert_eq!(restored.duration_ms, 20);
    }

    #[test]
    fn an_export_failure_does_not_lose_the_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(directory.path(), false).unwrap();
        store.record(result("a"), 10).unwrap();
        std::fs::create_dir(directory.path().join(PREDICTIONS_FILE)).unwrap();
        assert!(store.write_exports(&report(store.state())).is_err());
        drop(store);
        assert_eq!(
            open(directory.path(), true).unwrap().state().results.len(),
            1
        );
    }
}
