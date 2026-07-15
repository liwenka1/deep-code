//! Latest-wins persistence actor.
//!
//! All disk writes for the [`SessionRecord`] are funnelled through a single
//! background tokio task. Callers mutate the in-memory record under its
//! mutex and then call [`PersistenceActorHandle::request_save`], which is
//! non-blocking. The actor coalesces queued save requests so a burst of
//! rapid mutations collapses to one atomic write.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::session_store::{SessionRecord, SessionStore};

/// Backoff delays between save retries. A save is attempted once, then retried
/// after each of these delays before the failure is finally recorded. This
/// rides out transient faults (a momentary `ENOSPC`, a file lock, a network-FS
/// blip) without waiting for the next mutation — which might never come if the
/// user goes idle or the session ends right after the failed write.
const SAVE_RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(200),
    Duration::from_millis(800),
    Duration::from_millis(2000),
];

#[derive(Debug)]
enum Command {
    Save,
    Flush(oneshot::Sender<()>),
    #[cfg(test)]
    Shutdown(oneshot::Sender<()>),
}

/// Cheap-to-clone handle held by the runtime. Cloning shares the same actor
/// task; when the last handle drops the channel closes and the actor exits.
#[derive(Debug, Clone)]
pub(super) struct PersistenceActorHandle {
    tx: mpsc::UnboundedSender<Command>,
    /// Last save failure, cleared by the next successful save. Surfaced to
    /// UIs via `SessionUpdated.save_error` — a silent persistence failure
    /// would otherwise let the user believe their session is durable.
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl PersistenceActorHandle {
    /// Spawn the actor on the current tokio runtime.
    ///
    /// If no tokio runtime is available in this context (e.g., synchronous
    /// unit tests instantiating an [`AgentRuntime`] for state assertions),
    /// the handle's channel is closed: [`request_save`] and [`flush`] become
    /// no-ops. Production code always runs inside `#[tokio::main]` so the
    /// actor is always spawned there.
    pub(super) fn spawn(
        store: Arc<dyn SessionStore + Send + Sync>,
        record: Arc<Mutex<SessionRecord>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let last_error = Arc::new(std::sync::Mutex::new(None));
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(actor_loop(store, record, rx, Arc::clone(&last_error)));
            }
            Err(_) => drop(rx),
        }
        Self { tx, last_error }
    }

    /// The most recent save failure, if the latest save attempt failed.
    #[must_use]
    pub(super) fn last_save_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .expect("save error lock poisoned")
            .clone()
    }

    /// Signal that a save is desired. Non-blocking. Concurrent and queued
    /// requests collapse into a single write (latest-wins).
    pub(super) fn request_save(&self) {
        let _ = self.tx.send(Command::Save);
    }

    /// Wait until pending saves complete. The actor stays alive afterwards.
    pub(super) async fn flush(&self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Command::Flush(ack)).is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// Wait until pending saves complete and stop the actor.
    #[cfg(test)]
    pub(super) async fn shutdown(self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown(ack)).is_err() {
            return;
        }
        let _ = rx.await;
    }
}

async fn actor_loop(
    store: Arc<dyn SessionStore + Send + Sync>,
    record: Arc<Mutex<SessionRecord>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
) {
    while let Some(first) = rx.recv().await {
        let mut pending_save = false;
        let mut acks: Vec<oneshot::Sender<()>> = Vec::new();
        let mut shutdown = false;

        absorb(first, &mut pending_save, &mut acks, &mut shutdown);
        while let Ok(next) = rx.try_recv() {
            absorb(next, &mut pending_save, &mut acks, &mut shutdown);
        }

        if pending_save {
            let outcome = save_with_retry(store.as_ref(), &record).await;
            let mut slot = last_error.lock().expect("save error lock poisoned");
            match outcome {
                Ok(()) => *slot = None,
                Err(error) => {
                    eprintln!("session save failed after retries: {error}");
                    *slot = Some(error.to_string());
                }
            }
        }
        for ack in acks {
            let _ = ack.send(());
        }
        if shutdown {
            return;
        }
    }
}

/// Save the record, retrying transient failures on the [`SAVE_RETRY_BACKOFF`]
/// schedule. The record is re-snapshotted before every attempt, so a mutation
/// that lands mid-backoff is captured by the next try (latest-wins holds).
/// Returns the last error only once every attempt is exhausted.
async fn save_with_retry(
    store: &(dyn SessionStore + Send + Sync),
    record: &Mutex<SessionRecord>,
) -> Result<(), crate::session_store::SessionStoreError> {
    let mut attempt = 0usize;
    loop {
        let snapshot = record.lock().await.clone();
        match store.save(&snapshot) {
            Ok(()) => return Ok(()),
            // Only transient I/O faults (ENOSPC, file lock, network-FS blip)
            // are worth retrying; serialization / invalid-id / schema errors are
            // permanent, so fail fast instead of burning the backoff budget on
            // every save.
            Err(error) => match SAVE_RETRY_BACKOFF
                .get(attempt)
                .filter(|_| matches!(error, crate::session_store::SessionStoreError::Io { .. }))
            {
                Some(&delay) => {
                    eprintln!(
                        "session save failed (attempt {}), retrying in {}ms: {error}",
                        attempt + 1,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                None => return Err(error),
            },
        }
    }
}

fn absorb(
    command: Command,
    pending_save: &mut bool,
    acks: &mut Vec<oneshot::Sender<()>>,
    shutdown: &mut bool,
) {
    match command {
        Command::Save => *pending_save = true,
        Command::Flush(ack) => {
            *pending_save = true;
            acks.push(ack);
        }
        #[cfg(test)]
        Command::Shutdown(ack) => {
            *pending_save = true;
            acks.push(ack);
            *shutdown = true;
        }
    }
    #[cfg(not(test))]
    let _ = shutdown;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::AgentConfig;
    use crate::session_store::{SessionId, SessionRecord, SessionStoreError};

    #[derive(Default)]
    struct CountingStore {
        saves: AtomicUsize,
        latest: StdMutex<Option<SessionRecord>>,
    }

    impl CountingStore {
        fn saves(&self) -> usize {
            self.saves.load(Ordering::SeqCst)
        }
        fn latest(&self) -> Option<SessionRecord> {
            self.latest.lock().unwrap().clone()
        }
    }

    impl SessionStore for CountingStore {
        fn save(&self, record: &SessionRecord) -> Result<(), SessionStoreError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            *self.latest.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
            Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            })
        }
        fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
            Ok(Vec::new())
        }
        fn delete(&self, _id: &SessionId) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    struct FailingStore;
    impl SessionStore for FailingStore {
        fn save(&self, _record: &SessionRecord) -> Result<(), SessionStoreError> {
            Err(SessionStoreError::Io {
                message: "synthetic failure".to_string(),
            })
        }
        fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
            Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            })
        }
        fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
            Ok(Vec::new())
        }
        fn delete(&self, _id: &SessionId) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    fn make_record() -> SessionRecord {
        SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::default(), "system")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coalesces_burst_into_single_save() {
        let store = Arc::new(CountingStore::default());
        let store_dyn: Arc<dyn SessionStore + Send + Sync> = store.clone();
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store_dyn, Arc::clone(&record));

        // On current_thread we hold the executor across these sync sends, so
        // the actor cannot run until we yield. All 100 messages plus the
        // Flush command end up in the queue together.
        for _ in 0..100 {
            handle.request_save();
        }
        handle.flush().await;

        assert_eq!(store.saves(), 1, "burst should coalesce into one save");
        assert!(store.latest().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flush_waits_for_pending_save_to_complete() {
        let store = Arc::new(CountingStore::default());
        let store_dyn: Arc<dyn SessionStore + Send + Sync> = store.clone();
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store_dyn, Arc::clone(&record));

        handle.request_save();
        assert_eq!(
            store.saves(),
            0,
            "actor cannot have run yet on current_thread"
        );
        handle.flush().await;
        assert_eq!(store.saves(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_drains_pending_then_exits() {
        let store = Arc::new(CountingStore::default());
        let store_dyn: Arc<dyn SessionStore + Send + Sync> = store.clone();
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store_dyn, Arc::clone(&record));

        for _ in 0..5 {
            handle.request_save();
        }
        handle.shutdown().await;

        assert_eq!(
            store.saves(),
            1,
            "shutdown should drain pending burst into one final save"
        );
    }

    /// A store that fails its first `fail_first` save attempts, then succeeds.
    /// Time is virtual in these tests (`start_paused`), so retry backoff is
    /// instant.
    struct FlakyStore {
        attempts: AtomicUsize,
        fail_first: usize,
    }
    impl SessionStore for FlakyStore {
        fn save(&self, _record: &SessionRecord) -> Result<(), SessionStoreError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) < self.fail_first {
                Err(SessionStoreError::Io {
                    message: "disk full".to_string(),
                })
            } else {
                Ok(())
            }
        }
        fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
            Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            })
        }
        fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
            Ok(Vec::new())
        }
        fn delete(&self, _id: &SessionId) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn save_failure_does_not_kill_actor() {
        let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(FailingStore);
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store, Arc::clone(&record));

        handle.request_save();
        handle.flush().await;
        // Actor must still be alive — a second flush should still resolve.
        handle.flush().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn transient_failure_is_transparently_retried() {
        // One failure inside the retry budget is recovered within the same
        // save cycle: no error is ever surfaced.
        let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(FlakyStore {
            attempts: AtomicUsize::new(0),
            fail_first: 1,
        });
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store, Arc::clone(&record));

        handle.request_save();
        handle.flush().await;
        assert_eq!(
            handle.last_save_error(),
            None,
            "a transient failure within budget must not surface"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn save_failure_is_recorded_after_retries_and_cleared_on_recovery() {
        // Fail the whole first cycle (initial attempt + all backoff retries),
        // then succeed — the error is recorded, then cleared on the next cycle.
        let budget = SAVE_RETRY_BACKOFF.len() + 1;
        let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(FlakyStore {
            attempts: AtomicUsize::new(0),
            fail_first: budget,
        });
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store, Arc::clone(&record));

        handle.request_save();
        handle.flush().await;
        assert!(
            handle
                .last_save_error()
                .is_some_and(|error| error.contains("disk full")),
            "error must be recorded once every retry is exhausted"
        );

        handle.request_save();
        handle.flush().await;
        assert_eq!(handle.last_save_error(), None, "recovery clears the error");
    }

    /// A store whose every save fails with a permanent (non-I/O) error, counting
    /// attempts so the test can assert the retry budget was NOT spent.
    struct PermanentFailStore {
        attempts: Arc<AtomicUsize>,
    }
    impl SessionStore for PermanentFailStore {
        fn save(&self, _record: &SessionRecord) -> Result<(), SessionStoreError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(SessionStoreError::Serialization {
                message: "permanent".to_string(),
            })
        }
        fn load(&self, id: &SessionId) -> Result<SessionRecord, SessionStoreError> {
            Err(SessionStoreError::NotFound {
                id: id.as_str().to_string(),
            })
        }
        fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
            Ok(Vec::new())
        }
        fn delete(&self, _id: &SessionId) -> Result<(), SessionStoreError> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn permanent_failure_is_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(PermanentFailStore {
            attempts: Arc::clone(&attempts),
        });
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store, Arc::clone(&record));

        handle.request_save();
        handle.flush().await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a permanent error must fail fast, not consume the retry budget"
        );
        assert!(
            handle
                .last_save_error()
                .is_some_and(|error| error.contains("permanent")),
            "the permanent error is still recorded"
        );
    }
}
