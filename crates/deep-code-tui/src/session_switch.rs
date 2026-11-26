//! Prepare session switches off the terminal thread, then commit them in memory.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use deep_code_agent::{
    AgentConfig, AgentRuntime, CancellationToken, JsonSessionStore, LaunchedRuntime,
    PreparedRestore, SessionId, SessionRecord, WorkspaceRoots,
};
use tokio::sync::{oneshot, watch};

/// An unused candidate has never run tools or queued a save. Retiring its
/// supervisor and empty job store is sufficient; ordinary shutdown would
/// incorrectly save a session the user cancelled.
pub(crate) struct PreparedSessionSwitch {
    launch: Option<(LaunchedRuntime, AgentConfig)>,
    restore: Option<RestoreSwitch>,
}

pub(crate) struct RestoreSwitch {
    pub fork: PreparedRestore,
    pub workspace_files: Vec<String>,
}

struct SwitchTarget {
    roots: WorkspaceRoots,
    record: SessionRecord,
    restore: Option<RestoreSwitch>,
}

impl PreparedSessionSwitch {
    pub fn take(mut self) -> (LaunchedRuntime, AgentConfig, Option<RestoreSwitch>) {
        let (runtime, config) = self
            .launch
            .take()
            .expect("prepared switch is consumed once");
        (runtime, config, self.restore.take())
    }
}

impl Drop for PreparedSessionSwitch {
    fn drop(&mut self) {
        if let Some((launched, _)) = self.launch.take() {
            (launched.stop_hook)();
            launched.job_store.shutdown();
        }
        if let Some(restore) = self.restore.take() {
            // A prepared restore may contain a large tree. Dropping Esc's
            // abandoned result on the terminal thread must stay quick.
            let _ = std::thread::Builder::new()
                .name("discard-restore".into())
                .spawn(move || drop(restore));
        }
    }
}

pub(crate) struct SessionSwitch {
    cancel: CancellationToken,
    rx: oneshot::Receiver<Result<PreparedSessionSwitch, String>>,
}

impl SessionSwitch {
    pub fn start(
        current: Arc<AgentRuntime>,
        roots: WorkspaceRoots,
        global_config_path: Option<PathBuf>,
        record: SessionRecord,
    ) -> Result<Self, String> {
        Self::start_with(current, global_config_path, move |_| {
            Ok(SwitchTarget {
                roots,
                record,
                restore: None,
            })
        })
    }

    pub fn restore(
        current: Arc<AgentRuntime>,
        workspace: PathBuf,
        global_config_path: Option<PathBuf>,
        session: SessionId,
        selector: String,
    ) -> Result<Self, String> {
        Self::start_with(current, global_config_path, move |cancel| {
            let store = JsonSessionStore::for_pinned_workspace(&workspace)
                .map_err(|error| error.to_string())?;
            let fork = store
                .prepare_restore(&session, &selector, cancel)
                .map_err(|error| error.to_string())?;
            let record = fork.record().clone();
            let workspace_files = deep_code_agent::list_workspace_files(&record.workspace, 2000);
            Ok(SwitchTarget {
                roots: WorkspaceRoots::new(record.workspace.clone(), Vec::new()),
                record,
                restore: Some(RestoreSwitch {
                    fork,
                    workspace_files,
                }),
            })
        })
    }

    fn start_with(
        current: Arc<AgentRuntime>,
        global_config_path: Option<PathBuf>,
        target: impl FnOnce(&CancellationToken) -> Result<SwitchTarget, String> + Send + 'static,
    ) -> Result<Self, String> {
        let executor = tokio::runtime::Handle::try_current().ok();
        Self::spawn(move |cancel| {
            // Flush without shutting down jobs, subagents or LSP. If this
            // request is cancelled, the live runtime remains fully usable.
            drive(&executor, async {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err("session switch cancelled".to_string()),
                    result = async {
                        // A turn may have started before its UI event arrived.
                        current.live_snapshot().await.map_err(|error| error.to_string())?;
                        match current.flush_session().await {
                            Some(error) => Err(format!("could not save current session: {error}")),
                            None => Ok(()),
                        }
                    } => result,
                }
            })??;
            check_cancelled(&cancel)?;
            let SwitchTarget {
                roots,
                record,
                restore,
            } = target(&cancel)?;
            let config = AgentConfig::load_with(
                global_config_path,
                Some(roots.primary.join(".deep-code/config.toml")),
                &|name| std::env::var(name).ok(),
            )
            .config;
            check_cancelled(&cancel)?;
            // Production actors belong to the caller's executor, not to a
            // short-lived worker runtime. Synchronous callers retain the
            // normal no-executor behavior of runtime construction.
            let _entered = executor.as_ref().map(tokio::runtime::Handle::enter);
            let launched = deep_code_agent::prepare_runtime_resume(&config, roots, record)
                .map_err(|error| error.to_string())?;
            let prepared = PreparedSessionSwitch {
                launch: Some((launched, config)),
                restore,
            };
            check_cancelled(&cancel)?;
            Ok(prepared)
        })
    }

    fn spawn(
        prepare: impl FnOnce(CancellationToken) -> Result<PreparedSessionSwitch, String>
        + Send
        + 'static,
    ) -> Result<Self, String> {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("session-switch".to_string())
            .spawn(move || {
                // A dropped receiver also drops and retires a late candidate.
                let _ = tx.send(prepare(worker_cancel));
            })
            .map_err(|error| format!("could not start session switch: {error}"))?;
        Ok(Self { cancel, rx })
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    #[cfg(test)]
    pub fn is_ready(&self) -> bool {
        !self.rx.is_empty()
    }

    pub fn try_result(&mut self) -> Option<Result<PreparedSessionSwitch, String>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err("session switch stopped without a result".to_string()))
            }
        }
    }
}

impl Drop for SessionSwitch {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn check_cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("session switch cancelled".to_string())
    } else {
        Ok(())
    }
}

fn drive<T>(
    executor: &Option<tokio::runtime::Handle>,
    future: impl Future<Output = T>,
) -> Result<T, String> {
    match executor {
        Some(executor) => Ok(executor.block_on(future)),
        None => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
            .map(|runtime| runtime.block_on(future)),
    }
}

/// Post-commit work is tracked until it finishes, including on application
/// exit. Its completion can be polled without waiting for a directory lock.
pub(crate) struct SessionCleanup {
    done: watch::Receiver<Option<Result<(), String>>>,
    commit: Option<std::sync::mpsc::Sender<SaveCompletion>>,
}

type SaveCompletion = Pin<Box<dyn Future<Output = Option<String>> + Send>>;

impl SessionCleanup {
    pub fn prepare(old: Arc<AgentRuntime>) -> Result<Self, String> {
        let executor = tokio::runtime::Handle::try_current().ok();
        let (tx, done) = watch::channel(None);
        let (commit, committed) = std::sync::mpsc::channel::<SaveCompletion>();
        std::thread::Builder::new()
            .name("session-cleanup".to_string())
            .spawn(move || {
                let receipt = match committed.recv() {
                    Ok(receipt) => receipt,
                    Err(_) => {
                        let _ = tx.send(Some(Ok(())));
                        return;
                    }
                };
                let result = drive(&executor, async {
                    let (_, error) = tokio::join!(old.shutdown_after_flush(), receipt);
                    error.map_or(Ok(()), Err)
                })
                .and_then(|result| result);
                let _ = tx.send(Some(result));
            })
            .map_err(|error| format!("could not start session cleanup: {error}"))?;
        Ok(Self {
            done,
            commit: Some(commit),
        })
    }

    pub fn commit(&mut self, receipt: impl Future<Output = Option<String>> + Send + 'static) {
        if let Some(commit) = self.commit.take() {
            let _ = commit.send(Box::pin(receipt));
        }
    }

    pub fn try_result(&self) -> Option<Result<(), String>> {
        self.done.borrow().clone().or_else(|| {
            self.done
                .has_changed()
                .is_err()
                .then(|| Err("session cleanup stopped without a result".to_string()))
        })
    }

    pub async fn finish(&self) -> Result<(), String> {
        let mut done = self.done.clone();
        loop {
            if let Some(result) = done.borrow().clone() {
                return result;
            }
            done.changed()
                .await
                .map_err(|_| "session cleanup stopped without a result".to_string())?;
        }
    }
}
