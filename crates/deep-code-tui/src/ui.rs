mod input;
// `pub(crate)` for the sanitizer alone: `startup.rs` draws its own resume
// picker on a real backend before `App` exists, and it has to neutralize the
// same model- and repo-controlled strings this module does. Two renderers
// painting the same screen deserve one filter, not a copy each.
pub(crate) mod render;

use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::app::{App, LaunchConfig};

/// Redraws are coalesced to at most ~30fps; streaming deltas mark the UI
/// dirty instead of forcing a frame each.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// While streaming, repaint at least this often so the activity indicator
/// animates through a long time-to-first-token wait.
const STREAM_TICK_INTERVAL: Duration = Duration::from_millis(120);
type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(config: LaunchConfig) -> Result<()> {
    // The alternate-screen TUI owns the terminal; any stray write to stderr
    // (LSP "server unavailable" notices, persistence warnings, panics) would
    // paint raw text over the rendered buffer. Send stderr to a log file so it
    // can never corrupt the screen.
    redirect_stderr_to_log();
    // Must come after the redirect (so the default hook's message lands in the
    // log) and before raw mode (so a panic in `setup_terminal` is covered too).
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let mut app = App::launch(config);
    let result = run_loop(&mut terminal, &mut app);
    // Nothing may `?` past `shutdown_runtime`: an early return here skips the
    // final persist/flush and leaks job process trees and LSP children. The
    // loop result used to take that exit, and after that was fixed the
    // terminal-restore `?` still did. The loop error is reported first — it is
    // the root cause, a failed restore is at most a symptom.
    let restored = restore_terminal(&mut terminal);
    app.shutdown_runtime().await;
    result?;
    restored?;
    Ok(())
}

/// Restore the terminal on panic and put the message where the user can see it.
///
/// Three things conspire to make an unhandled panic invisible *and* leave the
/// terminal unusable: stderr is redirected to the log file above, the release
/// profile sets `panic = "abort"` (so no unwinding, no destructor, and
/// `restore_terminal` never runs), and raw mode + alternate screen + mouse
/// capture are all still active. Without this hook the user sees a frozen blank
/// terminal with no echo and has to blind-type `reset`. Hooks still run under
/// `panic = "abort"`, so this works in release.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
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

fn setup_terminal() -> Result<AppTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste: paste arrives as one `Event::Paste` string. Mouse
    // capture: wheel scrolls the transcript and drag selects text (we render
    // an in-app selection + copy, since capture disables native selection).
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    Ok(terminal)
}

fn restore_terminal(terminal: &mut AppTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    Ok(())
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

fn run_loop(terminal: &mut AppTerminal, app: &mut App) -> Result<()> {
    let mut needs_redraw = true;
    let mut last_draw: Option<Instant> = None;
    let mut was_streaming = app.is_streaming;

    while !app.should_quit {
        if app.drain_stream_updates() {
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
