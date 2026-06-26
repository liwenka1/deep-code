//! Latest-wins persistence actor.
//!
//! All disk writes for the [`SessionRecord`] are funnelled through a single
//! background tokio task. Callers mutate the in-memory record under its
//! mutex and then call [`PersistenceActorHandle::request_save`], which is
//! non-blocking. The actor coalesces queued save requests so a burst of
//! rapid mutations collapses to one atomic write.

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::session_store::{SessionRecord, SessionStore};

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
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(actor_loop(store, record, rx));
            }
            Err(_) => drop(rx),
        }
        Self { tx }
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
            let snapshot = record.lock().await.clone();
            if let Err(error) = store.save(&snapshot) {
                eprintln!("session save failed: {error}");
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

    #[tokio::test(flavor = "current_thread")]
    async fn save_failure_does_not_kill_actor() {
        let store: Arc<dyn SessionStore + Send + Sync> = Arc::new(FailingStore);
        let record = Arc::new(Mutex::new(make_record()));
        let handle = PersistenceActorHandle::spawn(store, Arc::clone(&record));

        handle.request_save();
        handle.flush().await;
        // Actor must still be alive — a second flush should still resolve.
        handle.flush().await;
    }
}
