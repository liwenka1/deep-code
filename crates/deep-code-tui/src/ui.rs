mod input;
// `pub(crate)` for the sanitizer alone: `startup.rs` draws its own resume
// picker on a real backend before `App` exists, and it has to neutralize the
// same model- and repo-controlled strings this module does. Two renderers
// painting the same screen deserve one filter, not a copy each.
pub(crate) mod render;

use crate::interruptible_output::Output;
use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use deep_code_agent::CancellationToken;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::app::{App, LaunchConfig};

/// Redraws are coalesced to at most ~30fps; streaming deltas mark the UI
/// dirty instead of forcing a frame each.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// While streaming, repaint at least this often so the activity indicator
/// animates through a long time-to-first-token wait.
const STREAM_TICK_INTERVAL: Duration = Duration::from_millis(120);
type AppTerminal = Terminal<CrosstermBackend<BufWriter<Output>>>;

/// Install once before startup and keep SIGINT routed through the scan,
/// picker and TUI. Tokio does not restore the default handler when a signal
/// receiver is dropped. Register synchronously on Unix so transitions cannot
/// lose an interrupt before the listener task is first polled.
pub(crate) fn watch_interrupt() -> io::Result<crate::interrupt::Interrupt> {
    crate::interrupt::Interrupt::watch()
}

pub async fn run(config: LaunchConfig, interrupted: &CancellationToken) -> Result<()> {
    // Configuration, prompt history and skill discovery can block in file I/O.
    // Wait interruptibly and enter raw mode only after initialization succeeds.
    let mut initializing = tokio::task::spawn_blocking(move || {
        // Keep background diagnostics off the eventual alternate screen. The
        // log itself is workspace I/O, so opening it also belongs on the worker.
        redirect_stderr_to_log();
        install_panic_hook();
        App::launch(config)
    });
    let mut app = tokio::select! {
        biased;
        () = interrupted.cancelled() => {
            // Dispose of a late result if the caller remains alive. No terminal
            // state has changed, so main can immediately exit with code 130.
            tokio::spawn(async move {
                if let Ok(app) = initializing.await {
                    app.shutdown_runtime().await;
                }
            });
            return Err(deep_code_agent::SessionStoreError::Cancelled.into());
        }
        result = &mut initializing => result?,
    };
    let (mut terminal, mut cleanup) = match setup_terminal(interrupted) {
        Ok(terminal) => terminal,
        Err(error) => {
            app.shutdown_runtime().await;
            return Err(error);
        }
    };
    let result = run_loop(&mut terminal, &mut app, interrupted);
    // Nothing may `?` past `shutdown_runtime`: an early return here skips the
    // final persist/flush and leaks job process trees and LSP children. The
    // loop result used to take that exit, and after that was fixed the
    // terminal-restore `?` still did. The loop error is reported first — it is
    // the root cause, a failed restore is at most a symptom.
    let restored = cleanup.restore();
    app.shutdown_runtime().await;
    if !interrupted.is_cancelled() {
        result?;
        restored?;
    }
    Ok(())
}

/// Restore the terminal on panic and put the message where the user can see it.
///
/// Three things conspire to make an unhandled panic invisible *and* leave the
/// terminal unusable: stderr is redirected to the log file above, the release
/// profile sets `panic = "abort"` (so no unwinding, no destructor, and
/// normal terminal cleanup never runs), and raw mode + alternate screen + mouse
/// capture are all still active. Without this hook the user sees a frozen blank
/// terminal with no echo and has to blind-type `reset`. Hooks still run under
/// `panic = "abort"`, so this works in release.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let Ok(stdout) = Output::stdout(&CancellationToken::new()) else {
            previous(info);
            return;
        };
        let mut stdout = BufWriter::new(stdout.cleanup());
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            TitleStack::Pop,
            crossterm::cursor::Show
        );
        // Written to stdout on purpose: stderr is the log file at this point.
        // English on purpose too: the hook is installed before any config (and
        // thus any language) exists, and the panic payload it frames is English
        // regardless.
        let _ = writeln!(stdout, "\ndeep-code panicked: {info}");
        // The default hook writes the message to the log unconditionally, but a
        // backtrace only when RUST_BACKTRACE is set — do not promise one.
        let _ = writeln!(
            stdout,
            "Details were written to .deep-code/deep-code.log \
             (run with RUST_BACKTRACE=1 to capture a backtrace there)."
        );
        let _ = stdout.flush();
        // Keep the default reporting too, so the log gets the full detail.
        previous(info);
    }));
}

fn setup_terminal(interrupted: &CancellationToken) -> Result<(AppTerminal, TerminalCleanup)> {
    let cleanup = TerminalCleanup::new(interrupted, true)?;
    let mut stdout = cleanup.writer();
    // Bracketed paste: paste arrives as one `Event::Paste` string. Mouse
    // capture: wheel scrolls the transcript and drag selects text (we render
    // an in-app selection + copy, since capture disables native selection).
    // Title: names the window or tab in terminals that show a title a program
    // sets. Not VS Code's tab, though: by default it is `${process}`, the
    // foreground process — for the npm install the `node` launcher, which leads
    // the process group — and an OSC title never reaches that label (measured
    // against VS Code's own node-pty). The shell's title is pushed first so
    // quitting can give it back.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        TitleStack::Push,
        SetTitle("deep-code")
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    Ok((terminal, cleanup))
}

/// Restores raw mode even if entering/drawing the terminal returns an error.
/// Escape-sequence delivery gets a fixed budget when the output has stalled.
pub(crate) struct TerminalCleanup {
    output: Output,
    title: bool,
    active: bool,
}

impl TerminalCleanup {
    pub fn new(interrupted: &CancellationToken, title: bool) -> io::Result<Self> {
        let output = Output::stdout(interrupted)?;
        enable_raw_mode()?;
        Ok(Self {
            output,
            title,
            active: true,
        })
    }

    pub fn writer(&self) -> BufWriter<Output> {
        BufWriter::new(self.output.clone())
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let raw = disable_raw_mode();
        let mut output = BufWriter::new(self.output.cleanup());
        let display = execute!(
            output,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        if self.title {
            let _ = execute!(output, TitleStack::Pop);
        }
        self.output.discard_terminal_writes();
        raw.and(display)
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// The terminal's title stack (XTWINOPS 22/23): push the title the shell had
/// before setting ours, pop it on the way out. Without the pop the tab keeps
/// reading `deep-code` after quit until something else sets a title, and most
/// shells never do. A terminal without the stack ignores both sequences and
/// keeps our title — what it showed before this existed. VS Code is one: its
/// xterm.js implements the stack but leaves it switched off (`windowOptions`).
enum TitleStack {
    Push,
    Pop,
}

impl crossterm::Command for TitleStack {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(match self {
            Self::Push => "\x1b[22;0t",
            Self::Pop => "\x1b[23;0t",
        })
    }

    // A console without VT sequences has no title stack to push to.
    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Where the log may be opened — `None` when the path is not ours to write.
///
/// This runs on every launch, before anything else, and then `dup2`s the result
/// onto fd 2: from that point the whole process's stderr — LSP noise,
/// persistence errors, panic payloads — flows into whatever it opened. Both
/// halves of the path were previously taken on trust:
///
/// * `.deep-code` was created with a bare `create_dir_all`, which follows a
///   symlink at that component, so a repository shipping `.deep-code` as a link
///   moved the log (and the session transcripts beside it) outside the
///   workspace;
/// * the leaf was opened `create(true).append(true)`, which follows a link
///   there too — so `.deep-code/deep-code.log → ~/.zshrc` turned every launch
///   into an unbounded append to a file that already existed and gets executed.
///
/// Planting either link is an ordinary permitted write inside a granted root,
/// so no sandbox refuses it, and this process is not sandboxed anyway. It is
/// the same rule already enforced for the spill files and `write_self_ignore`;
/// this was the last writer of that directory not obeying it.
///
/// On unix the refusal is also stated to the kernel with `O_NOFOLLOW`, which
/// makes it atomic. Windows `OpenOptions` has no equivalent flag, so there the
/// check-then-open window remains — narrower than the original hole by the
/// whole directory level, and knowingly left rather than papered over.
#[cfg(any(unix, windows))]
fn open_log_path() -> Option<std::path::PathBuf> {
    open_log_path_in(&crate::cli::workspace_root())
}

/// Split from [`open_log_path`] purely so it can be tested: the caller reads
/// process-global state, this half takes the root as an argument.
#[cfg(any(unix, windows))]
fn open_log_path_in(workspace: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = workspace.join(".deep-code");
    deep_code_agent::ensure_owned_dirs(&dir, 1).ok()?;
    let path = dir.join("deep-code.log");
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return None;
    }
    Some(path)
}

/// Point the process's stderr at `.deep-code/deep-code.log` so background tasks
/// (LSP, persistence) and panics can't paint over the alternate-screen TUI.
/// Best-effort: if the log can't be opened, stderr is left as-is.
#[cfg(unix)]
fn redirect_stderr_to_log() {
    use std::os::unix::io::AsRawFd;

    let Some(path) = open_log_path() else {
        return;
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let Ok(file) = options.open(&path) else {
        return;
    };
    // SAFETY: `file` holds a valid fd for the duration of this call, and
    // `STDERR_FILENO` (2) is always a valid target. After dup2, fd 2 is an
    // independent reference to the log file, so dropping `file` is fine.
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
}

/// Windows equivalent of the unix path: point the process's `STD_ERROR_HANDLE`
/// at the log file via `SetStdHandle`, so LSP/persistence `eprintln!`s land in
/// the log instead of corrupting the alternate-screen TUI.
#[cfg(windows)]
fn redirect_stderr_to_log() {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    let Some(path) = open_log_path() else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let handle: HANDLE = file.as_raw_handle();
    // SAFETY: `handle` is a valid, writable file handle. SetStdHandle just
    // records it as the process's stderr; `forget` keeps it open for the
    // process lifetime (the alternate-screen TUI runs until exit), mirroring how
    // the unix `dup2` fd outlives the dropped `File`.
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    std::mem::forget(file);
}

#[cfg(not(any(unix, windows)))]
fn redirect_stderr_to_log() {}

fn run_loop(
    terminal: &mut AppTerminal,
    app: &mut App,
    interrupted: &CancellationToken,
) -> Result<()> {
    let mut needs_redraw = true;
    let mut last_draw: Option<Instant> = None;
    let mut was_streaming = app.is_streaming;

    while !app.should_quit && !interrupted.is_cancelled() {
        if app.drain_stream_updates() {
            needs_redraw = true;
        }
        if app.poll_session_scan() {
            needs_redraw = true;
        }

        // A turn that just ended may have spawned child processes (shell, git,
        // LSP) that, on Windows, reset the console input mode and silently drop
        // our mouse capture — after which the terminal translates wheel motion
        // into ↑/↓ keys. Re-assert mouse capture on every turn boundary.
        if was_streaming && !app.is_streaming {
            let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
        }
        was_streaming = app.is_streaming;

        // While streaming, redraw on a slow tick even when no tokens arrived,
        // so the "generating Ns" activity indicator keeps animating during a
        // long time-to-first-token wait instead of looking frozen.
        let draw_due = last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW_INTERVAL);
        let tick_due =
            app.is_streaming && last_draw.is_none_or(|at| at.elapsed() >= STREAM_TICK_INTERVAL);
        if (needs_redraw && draw_due) || tick_due {
            app.enforce_history_cap();
            terminal.draw(|frame| render::render(frame, app))?;
            last_draw = Some(Instant::now());
            needs_redraw = false;
        }
        // After the draw, when the block a `/find` opened is in the snapshot.
        if app.run_pending_find() {
            needs_redraw = true;
        }
        // After the draw, so the screen already shows what the bell is for.
        if let Some(kind) = app.take_notification() {
            let _ = execute!(terminal.backend_mut(), Print(kind.terminal_sequence()));
        }

        if event::poll(Duration::from_millis(40))? {
            let event = event::read()?;
            needs_redraw |= input::dispatch_terminal_event(app, terminal, event)?;
        }
    }

    Ok(())
}

/// Soft upper bound for composer visible rows before scrolling.
pub(crate) const COMPOSER_MAX_VISIBLE_ROWS: usize = 6;

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::open_log_path_in;

    #[cfg(unix)]
    #[test]
    fn sigint_interrupts_blocked_startup_io_before_raw_mode() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD: &str = "DEEP_CODE_TUI_STARTUP_IO_TEST";
        if let Ok(intent) = std::env::var(CHILD) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let result: anyhow::Result<()> = runtime.block_on(async {
                let interrupted = super::watch_interrupt()?;
                let workspace = std::env::current_dir()?;
                let store_workspace = workspace.clone();
                let store = crate::startup::blocking_io(&interrupted, move || {
                    deep_code_agent::JsonSessionStore::for_workspace(store_workspace)
                })
                .await??;
                let intent = match intent.as_str() {
                    "continue" => crate::cli::StartupIntent::ContinueLatest,
                    "picker" => crate::cli::StartupIntent::ResumePicker,
                    _ => crate::cli::StartupIntent::New,
                };
                let resume =
                    crate::startup::choose_startup(&store, intent, &workspace, &interrupted)
                        .await?;
                super::run(
                    crate::app::LaunchConfig {
                        workspace: Some(workspace),
                        resume,
                        ..Default::default()
                    },
                    &interrupted,
                )
                .await
            });
            assert!(matches!(
                result
                    .unwrap_err()
                    .downcast_ref::<deep_code_agent::SessionStoreError>(),
                Some(deep_code_agent::SessionStoreError::Cancelled)
            ));
            // Like main, do not wait for the blocked worker during runtime drop.
            std::process::exit(130);
        }

        for intent in ["new", "continue", "picker"] {
            for blocked in [".deep-code/config.toml", "skills/blocked/SKILL.md"] {
                let workspace = tempfile::tempdir().unwrap();
                let path = workspace.path().join(blocked);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: valid NUL-terminated path in this test's workspace.
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "ui::tests::sigint_interrupts_blocked_startup_io_before_raw_mode",
                    ])
                    .env(CHILD, intent)
                    .current_dir(workspace.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                // Wait until the actual startup reader opens the FIFO. Keeping
                // the writer open without data blocks the read indefinitely.
                let writer = loop {
                    if let Ok(writer) = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&path)
                    {
                        break writer;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let output = child.wait_with_output().unwrap();
                        panic!("{intent}: startup never reached {blocked}: {output:?}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                };
                // SAFETY: the PID belongs to this test's live child process.
                assert_eq!(
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
                    0
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("{intent}: SIGINT was ignored while reading {blocked}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let output = child.wait_with_output().unwrap();
                drop(writer);
                assert_eq!(output.status.code(), Some(130), "{output:?}");
                assert!(
                    !output
                        .stdout
                        .windows(8)
                        .any(|bytes| bytes == b"\x1b[?1049h")
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn sigint_stays_handled_after_startup_scan() {
        // A real SIGINT must target an isolated process, not the test runner.
        const CHILD: &str = "DEEP_CODE_SIGINT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ui::tests::sigint_stays_handled_after_startup_scan",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "SIGINT regression child failed: {status}");
            return;
        }

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let interrupted = super::watch_interrupt().unwrap();
                let workspace = tempfile::tempdir().unwrap();
                let store =
                    deep_code_agent::JsonSessionStore::for_workspace(workspace.path()).unwrap();
                for intent in [
                    crate::cli::StartupIntent::ContinueLatest,
                    crate::cli::StartupIntent::ResumePicker,
                ] {
                    crate::startup::choose_startup(&store, intent, workspace.path(), &interrupted)
                        .await
                        .unwrap();
                }
                // Both startup scans have returned, but the UI's shared token
                // must still receive an external interrupt.
                // SAFETY: this is the isolated child and the handler is installed.
                assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
                tokio::time::timeout(std::time::Duration::from_secs(5), interrupted.cancelled())
                    .await
                    .expect("SIGINT was ignored after startup");
            });
    }

    /// Cross-platform half of the guard: `.deep-code` has to be a real
    /// directory. Anything else there is not ours to write under, and the old
    /// bare `create_dir_all` + open would have gone straight through it.
    #[test]
    fn a_state_dir_that_is_not_a_directory_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join(".deep-code"), "not a directory").unwrap();

        assert!(
            open_log_path_in(workspace.path()).is_none(),
            "stderr was pointed at a path under a non-directory .deep-code"
        );
    }

    #[test]
    fn a_real_state_dir_yields_the_log_path() {
        let workspace = tempfile::tempdir().unwrap();

        let path = open_log_path_in(workspace.path()).expect("a clean workspace must be usable");

        assert_eq!(path, workspace.path().join(".deep-code/deep-code.log"));
        assert!(
            path.parent().unwrap().is_dir(),
            "the log directory must have been created"
        );
    }

    /// The leaf. `create(true).append(true)` follows a symlink, so
    /// `.deep-code/deep-code.log -> ~/.zshrc` turned every launch into an
    /// unbounded append onto a file that already existed — panics, LSP output,
    /// persistence errors, all of it, from the unsandboxed parent process.
    ///
    /// `#[cfg(unix)]` because `crate::test_symlinks` lives in the agent crate
    /// behind `#[cfg(test)]` and so is not reachable from here; the sibling
    /// test above covers the directory half on every platform.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_log_is_refused_rather_than_appended_through() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("shell-rc");
        std::fs::write(&victim, "# original\n").unwrap();
        let state_dir = workspace.path().join(".deep-code");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::os::unix::fs::symlink(&victim, state_dir.join("deep-code.log")).unwrap();

        assert!(
            open_log_path_in(workspace.path()).is_none(),
            "stderr was pointed through a symlink at {}",
            victim.display()
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "# original\n");
    }

    /// And the directory half of the same escape: a symlinked `.deep-code`
    /// relocates the log — and the session transcripts beside it — wholesale.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_state_dir_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join(".deep-code")).unwrap();

        assert!(
            open_log_path_in(workspace.path()).is_none(),
            "stderr was pointed outside the workspace"
        );
        assert!(!outside.path().join("deep-code.log").exists());
    }
}
