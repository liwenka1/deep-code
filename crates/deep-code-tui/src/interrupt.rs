//! One interruption listener for the entire interactive/headless command.

use deep_code_agent::CancellationToken;

pub(crate) struct Interrupt {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Interrupt {
    pub fn watch() -> std::io::Result<Self> {
        #[cfg(unix)]
        let (mut int, mut term, mut hup) = {
            use tokio::signal::unix::{SignalKind, signal};
            // Register before returning, including before any synchronous I/O.
            let hup = if signal_is_ignored(libc::SIGHUP) {
                None
            } else {
                Some(signal(SignalKind::hangup())?)
            };
            (
                signal(SignalKind::interrupt())?,
                signal(SignalKind::terminate())?,
                hup,
            )
        };
        let token = CancellationToken::new();
        let notify = token.clone();
        let task = tokio::spawn(async move {
            #[cfg(unix)]
            tokio::select! {
                _ = int.recv() => {},
                _ = term.recv() => {},
                () = async {
                    match hup.as_mut() {
                        Some(hup) => { hup.recv().await; },
                        None => std::future::pending().await,
                    }
                } => {},
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            notify.cancel();
        });
        Ok(Self { token, task })
    }
}

impl std::ops::Deref for Interrupt {
    type Target = CancellationToken;
    fn deref(&self) -> &Self::Target {
        &self.token
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Preserve nohup's inherited SIG_IGN rather than replacing it with a handler.
#[cfg(unix)]
pub(crate) fn signal_is_ignored(signum: libc::c_int) -> bool {
    // SAFETY: the null action queries the disposition without changing it.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signum, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}
