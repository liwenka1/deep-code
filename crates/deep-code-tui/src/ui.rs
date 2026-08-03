mod input;
mod render;

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
        let _ = writeln!(stdout, "\ndeep-code panicked: {info}");
        let _ = writeln!(
            stdout,
            "A backtrace was written to .deep-code/deep-code.log"
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

/// Point the process's stderr at `.deep-code/deep-code.log` so background tasks
/// (LSP, persistence) and panics can't paint over the alternate-screen TUI.
/// Best-effort: if the log can't be opened, stderr is left as-is.
#[cfg(unix)]
fn redirect_stderr_to_log() {
    use std::os::unix::io::AsRawFd;

    let path = crate::cli::workspace_root()
        .join(".deep-code")
        .join("deep-code.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
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

    let path = crate::cli::workspace_root()
        .join(".deep-code")
        .join("deep-code.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
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
