//! Lossless, bounded output with cancellation independent of a blocked OS write.
//!
//! A detached writer owns the descriptor; callers wait for acknowledgements,
//! never for an uninterruptible write or thread join. Each request is at most
//! 64 KiB and only one request can wait behind the active write.

use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use deep_code_agent::CancellationToken;
use tokio::sync::{mpsc, oneshot};

struct Request {
    bytes: Vec<u8>,
    done: oneshot::Sender<io::Result<()>>,
}

#[derive(Clone)]
pub(crate) struct Output {
    tx: mpsc::Sender<Request>,
    interrupted: CancellationToken,
    deadline: Option<Instant>,
    discarded: Arc<AtomicBool>,
}

impl Output {
    pub fn new(
        mut writer: impl Write + Send + 'static,
        interrupted: &CancellationToken,
    ) -> io::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<Request>(1);
        std::thread::Builder::new()
            .name("cli-output".into())
            .spawn(move || {
                while let Some(request) = rx.blocking_recv() {
                    // Don't begin an abandoned write after cancellation.
                    if request.done.is_closed() {
                        continue;
                    }
                    let result = writer
                        .write_all(&request.bytes)
                        .and_then(|()| writer.flush());
                    let failed = result.is_err();
                    let _ = request.done.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            interrupted: interrupted.clone(),
            deadline: None,
            discarded: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn stdout(interrupted: &CancellationToken) -> io::Result<Self> {
        Self::new(stdout_writer()?, interrupted)
    }

    pub fn stderr(interrupted: &CancellationToken) -> io::Result<Self> {
        Self::new(stderr_writer()?, interrupted)
    }

    /// Use one wall-clock budget for every write in a headless turn, including
    /// its initial envelope and final report.
    pub fn with_deadline(&self, deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            ..self.clone()
        }
    }

    /// Cleanup may still write after cancellation, but must never hold up exit.
    pub fn cleanup(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            interrupted: CancellationToken::new(),
            deadline: Some(Instant::now() + Duration::from_millis(200)),
            discarded: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.interrupted.cancel();
    }

    /// After terminal cleanup, BufWriter/ratatui destructors must not retry
    /// stale frames or print their own synchronous error messages to stderr.
    pub fn discard_terminal_writes(&self) {
        self.discarded.store(true, Ordering::Relaxed);
    }

    fn check(&self) -> io::Result<()> {
        if self.interrupted.is_cancelled() {
            // Write::write_all retries Interrupted, so use a non-retryable kind.
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "output cancelled",
            ));
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "output timed out"));
        }
        Ok(())
    }

    async fn stopped(&self) {
        match self.deadline {
            Some(deadline) => tokio::select! {
                () = self.interrupted.cancelled() => {},
                () = tokio::time::sleep_until(deadline.into()) => {},
            },
            None => self.interrupted.cancelled().await,
        }
    }

    pub async fn send(&self, bytes: &[u8]) -> io::Result<()> {
        for bytes in bytes.chunks(64 * 1024) {
            self.check()?;
            let (done, received) = oneshot::channel();
            tokio::select! {
                biased;
                () = self.stopped() => return self.check(),
                result = async {
                    self.tx.send(Request { bytes: bytes.to_vec(), done }).await.map_err(|_| closed())?;
                    received.await.map_err(|_| closed())?
                } => result?,
            }
        }
        Ok(())
    }

    fn send_sync(&self, bytes: &[u8]) -> io::Result<()> {
        if self.discarded.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.check()?;
        let (done, mut received) = oneshot::channel();
        let mut request = Request {
            bytes: bytes.to_vec(),
            done,
        };
        loop {
            self.check()?;
            match self.tx.try_send(request) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Full(pending)) => request = pending,
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(closed()),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        loop {
            self.check()?;
            match received.try_recv() {
                Ok(result) => return result,
                Err(oneshot::error::TryRecvError::Closed) => return Err(closed()),
                Err(oneshot::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
    }
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(64 * 1024);
        self.send_sync(&bytes[..count])?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_sync(&[])
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "output writer closed")
}

// Do not hold Rust's global stdout/stderr lock on a blocked worker: process
// exit flushes stdout and would deadlock trying to acquire that same lock.
#[cfg(unix)]
fn duplicate(fd: std::os::fd::RawFd) -> io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl validates the descriptor (including a closed stdout) and
    // creates an owned copy that won't leak into launched tools. It does not
    // change the inherited descriptor's flags.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: this new descriptor is transferred to exactly one File.
        Ok(unsafe { std::fs::File::from_raw_fd(copy) })
    }
}

pub(crate) fn stdout_writer() -> io::Result<impl Write + Send + 'static> {
    #[cfg(unix)]
    {
        duplicate(libc::STDOUT_FILENO)
    }
    #[cfg(windows)]
    {
        use std::io::IsTerminal;
        use std::os::windows::io::AsHandle;
        let stdout = std::io::stdout();
        // Preserve Rust's UTF-8 -> WriteConsoleW conversion on native consoles.
        // Redirected pipes must use an independent handle, without stdio locks.
        if stdout.is_terminal() {
            Ok(Box::new(ConsoleWriter {
                writer: stdout,
                pending: Vec::new(),
            }) as Box<dyn Write + Send>)
        } else {
            Ok(Box::new(std::fs::File::from(
                stdout.as_handle().try_clone_to_owned()?,
            )) as Box<dyn Write + Send>)
        }
    }
}

pub(crate) fn stderr_writer() -> io::Result<impl Write + Send + 'static> {
    #[cfg(unix)]
    {
        duplicate(libc::STDERR_FILENO)
    }
    #[cfg(windows)]
    {
        use std::io::IsTerminal;
        use std::os::windows::io::AsHandle;
        let stderr = std::io::stderr();
        if stderr.is_terminal() {
            Ok(Box::new(ConsoleWriter {
                writer: stderr,
                pending: Vec::new(),
            }) as Box<dyn Write + Send>)
        } else {
            Ok(Box::new(std::fs::File::from(
                stderr.as_handle().try_clone_to_owned()?,
            )) as Box<dyn Write + Send>)
        }
    }
}

// Windows' native console requires complete UTF-8. Request/BufWriter chunk
// boundaries may split a character, so retain at most its unfinished suffix.
#[cfg(any(windows, test))]
struct ConsoleWriter<W> {
    writer: W,
    pending: Vec<u8>,
}

#[cfg(any(windows, test))]
impl<W: Write> Write for ConsoleWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        let complete = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        self.writer.write_all(&self.pending[..complete])?;
        self.pending.drain(..complete);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc as sync_mpsc};

    struct Writer {
        bytes: Arc<Mutex<Vec<u8>>>,
        gate: Option<(sync_mpsc::Sender<()>, sync_mpsc::Receiver<()>)>,
    }

    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some((ready, release)) = self.gate.take() {
                ready.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            // Short writes exercise write_all, chunk boundaries and ordering.
            let count = bytes.len().min(997);
            self.bytes
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn native_console_receives_complete_utf8_even_across_flushes() {
        struct StrictUtf8(Vec<u8>);
        impl Write for StrictUtf8 {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                std::str::from_utf8(bytes).unwrap();
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ConsoleWriter {
            writer: StrictUtf8(Vec::new()),
            pending: Vec::new(),
        };
        let text = "取消🙂\n";
        for byte in text.as_bytes() {
            writer.write_all(&[*byte]).unwrap();
            writer.flush().unwrap();
            assert!(writer.pending.len() <= 3);
        }
        assert_eq!(writer.writer.0, text.as_bytes());
    }

    #[tokio::test]
    async fn output_is_lossless_across_short_writes_and_chunk_boundaries() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut output = Output::new(
            Writer {
                bytes: bytes.clone(),
                gate: None,
            },
            &CancellationToken::new(),
        )
        .unwrap();
        let expected = "中🙂x".repeat(30_000).into_bytes();
        output.send(&expected).await.unwrap();
        output.write_all(b"terminal frame").unwrap();
        output.flush().unwrap();
        assert_eq!(
            *bytes.lock().unwrap(),
            [expected, b"terminal frame".to_vec()].concat()
        );
    }

    #[tokio::test]
    async fn cancellation_and_cleanup_never_wait_for_a_blocked_writer() {
        let token = CancellationToken::new();
        let (ready_tx, ready) = sync_mpsc::channel();
        let (release, release_rx) = sync_mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Output::new(
            Writer {
                bytes,
                gate: Some((ready_tx, release_rx)),
            },
            &token,
        )
        .unwrap();
        let pending = tokio::spawn({
            let output = output.clone();
            async move { output.send(&vec![1; 256 * 1024]).await }
        });
        tokio::task::spawn_blocking(move || ready.recv_timeout(Duration::from_secs(5)).unwrap())
            .await
            .unwrap();
        token.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        // Both the synchronous renderer and asynchronous reports must stop.
        assert_eq!(
            output.clone().write_all(b"frame").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let cleanup = output.cleanup();
        let started = Instant::now();
        assert_eq!(
            cleanup.send(b"cancelled").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(output.tx.max_capacity() <= 1);
        release.send(()).unwrap();
    }

    #[test]
    fn synchronous_terminal_write_observes_cancellation_from_signal_thread() {
        let token = CancellationToken::new();
        let (ready_tx, ready) = sync_mpsc::channel();
        let (release, release_rx) = sync_mpsc::channel();
        let mut output = Output::new(
            Writer {
                bytes: Default::default(),
                gate: Some((ready_tx, release_rx)),
            },
            &token,
        )
        .unwrap();
        let waiting = std::thread::spawn(move || output.write_all(&vec![0; 128 * 1024]));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        token.cancel();
        assert!(waiting.join().unwrap().is_err());
        release.send(()).unwrap();
    }
}
