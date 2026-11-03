//! A cancellable directory scan shared by interactive and startup callers.

use deep_code_agent::{CancellationToken, JsonSessionStore, SessionRecord, SessionStoreError};
use tokio::sync::oneshot;

pub(crate) struct SessionListing {
    pub records: Vec<SessionRecord>,
    pub diagnostics: Vec<String>,
}

/// Dropping a pending request cancels its worker. A dedicated thread also
/// works before the TUI starts and in synchronous callers; it never holds up
/// the terminal loop or Tokio's executor/shutdown waiting for a whole scan.
pub(crate) struct SessionScan {
    cancel: CancellationToken,
    rx: oneshot::Receiver<Result<SessionListing, SessionStoreError>>,
}

impl SessionScan {
    pub fn start(store: JsonSessionStore) -> Result<Self, SessionStoreError> {
        Self::spawn(move |cancel| {
            let records = store.list_cancellable(&cancel)?;
            Ok(SessionListing {
                records,
                diagnostics: store.take_diagnostics(),
            })
        })
    }

    pub(crate) fn spawn(
        scan: impl FnOnce(CancellationToken) -> Result<SessionListing, SessionStoreError>
        + Send
        + 'static,
    ) -> Result<Self, SessionStoreError> {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("session-scan".to_string())
            .spawn(move || {
                let _ = tx.send(scan(worker_cancel));
            })
            .map_err(|error| SessionStoreError::Io {
                message: format!("could not start session scan: {error}"),
            })?;
        Ok(Self { cancel, rx })
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn try_result(&mut self) -> Option<Result<SessionListing, SessionStoreError>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(worker_stopped())),
        }
    }

    pub async fn finish(&mut self) -> Result<SessionListing, SessionStoreError> {
        (&mut self.rx).await.map_err(|_| worker_stopped())?
    }
}

impl Drop for SessionScan {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn worker_stopped() -> SessionStoreError {
    SessionStoreError::Io {
        message: "session scan stopped without a result".to_string(),
    }
}

pub(crate) async fn wait_with_interrupt(
    mut scan: SessionScan,
    interrupt: impl std::future::Future<Output = ()>,
) -> Result<SessionListing, SessionStoreError> {
    tokio::select! {
        biased;
        _ = interrupt => Err(SessionStoreError::Cancelled),
        result = scan.finish() => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn startup_wait_services_interrupts_while_the_worker_is_blocked() {
        let (started, ready) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, done) = oneshot::channel();
        let scan = SessionScan::spawn(move |cancel| {
            let _ = started.send(cancel.clone());
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            let _ = finished.send(());
            Err(SessionStoreError::Cancelled)
        })
        .unwrap();
        let cancel = ready.await.unwrap();
        let (interrupt, interrupted) = oneshot::channel();
        let waiting = tokio::spawn(wait_with_interrupt(scan, async {
            let _ = interrupted.await;
        }));
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        interrupt.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(SessionStoreError::Cancelled)));
        assert!(
            cancel.is_cancelled(),
            "dropping the wait must stop its worker"
        );
        release.send(()).unwrap();
        done.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_wait_delivers_complete_results() {
        let scan = SessionScan::spawn(|_| {
            Ok(SessionListing {
                records: Vec::new(),
                diagnostics: vec!["a skipped file".to_string()],
            })
        })
        .unwrap();
        let listing = wait_with_interrupt(scan, std::future::pending())
            .await
            .unwrap();
        assert!(listing.records.is_empty());
        assert_eq!(listing.diagnostics, ["a skipped file"]);
    }
}
