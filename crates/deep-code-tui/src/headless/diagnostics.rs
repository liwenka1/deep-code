//! Bounded stderr delivery that never stalls the headless signal/event loop.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};

enum Message {
    Line(String),
    Flush(oneshot::Sender<()>),
}

pub(super) struct Diagnostics {
    tx: mpsc::Sender<Message>,
    omitted: Arc<AtomicUsize>,
}

impl Diagnostics {
    pub fn new(mut out: impl Write + Send + 'static) -> std::io::Result<Self> {
        // Bound queued diagnostics to about 4 MiB even when stderr stops reading.
        let (tx, mut rx) = mpsc::channel(64);
        let omitted = Arc::new(AtomicUsize::new(0));
        let skipped = omitted.clone();
        // A detached thread may remain blocked in an OS write after cancellation.
        // It must not hold up Tokio shutdown or require a join before CLI exit.
        std::thread::Builder::new()
            .name("headless-stderr".into())
            .spawn(move || {
                while let Some(message) = rx.blocking_recv() {
                    let count = skipped.swap(0, Ordering::Relaxed);
                    if count != 0 {
                        let _ = writeln!(
                            out,
                            "diagnostics: {count} lines omitted while stderr was busy"
                        );
                    }
                    match message {
                        Message::Line(line) => {
                            let _ = writeln!(out, "{line}");
                        }
                        Message::Flush(done) => {
                            let _ = out.flush();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { tx, omitted })
    }

    pub fn line(&self, line: impl Into<String>) {
        let mut line = line.into();
        const MAX_LINE_BYTES: usize = 64 * 1024;
        if line.len() > MAX_LINE_BYTES {
            let mut end = MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str(" [diagnostic truncated]");
            line.shrink_to_fit();
        }
        if self.tx.try_send(Message::Line(line)).is_err() {
            self.omitted.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Callers select this against interruption; cancellation never waits for
    /// stderr space, including space for the cancellation notice itself.
    pub async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.tx.send(Message::Flush(done)).await.is_ok() {
            let _ = flushed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc as sync_mpsc};
    use std::time::Duration;

    struct BlockedWriter {
        started: Option<sync_mpsc::Sender<()>>,
        release: sync_mpsc::Receiver<()>,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_stderr_keeps_queue_and_cancellation_bounded() {
        let (started, ready) = sync_mpsc::channel();
        let (release, blocked) = sync_mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = Diagnostics::new(BlockedWriter {
            started: Some(started),
            release: blocked,
            bytes: bytes.clone(),
        })
        .unwrap();
        diagnostics.line("first");
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..100 {
            diagnostics.line("queued");
        }
        let omitted = diagnostics.omitted.load(Ordering::Relaxed);
        assert!(omitted > 0);
        // Even a flush waiting for queue space must yield to cancellation.
        tokio::select! {
            () = diagnostics.flush() => panic!("blocked output flushed unexpectedly"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), diagnostics.flush())
            .await
            .unwrap();
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(text.starts_with("first\n"));
        assert!(text.contains(&format!("{omitted} lines omitted")));
    }

    #[tokio::test]
    async fn normal_diagnostics_are_flushed_in_order_and_long_lines_are_bounded() {
        let (_, release) = sync_mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = Diagnostics::new(BlockedWriter {
            started: None,
            release,
            bytes: bytes.clone(),
        })
        .unwrap();
        diagnostics.line("one");
        diagnostics.line("two");
        diagnostics.line("😀".repeat(20_000));
        diagnostics.flush().await;
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(text.starts_with("one\ntwo\n"));
        assert!(text.ends_with(" [diagnostic truncated]\n"));
        assert!(text.len() < 66_000);
    }
}
