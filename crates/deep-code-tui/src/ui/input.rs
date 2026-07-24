//! Terminal event routing and key handling: turns crossterm events into
//! [`App`] state mutations, including the Windows paste-burst heuristic.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;

use crate::app::App;
use deep_code_agent::i18n::{TextId, tr_with};

use super::AppTerminal;

/// Route one terminal event; returns whether a redraw is needed.
pub(super) fn dispatch_terminal_event(
    app: &mut App,
    terminal: &mut AppTerminal,
    event: Event,
) -> Result<bool> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // Windows fallback: crossterm's console event source does not
            // deliver bracketed paste there, so a paste replays as a flood of
            // already-queued key events — and every newline in the pasted
            // text would hit the Enter/submit path. If more keys are queued
            // at zero delay behind this one, gather the burst and decide
            // whether it is a paste. On unix, real pastes arrive as
            // `Event::Paste`, so this path stays cold.
            //
            // On Windows, crossterm's background reader thread reads from
            // the console in batches; `poll(Duration::ZERO)` can return
            // false while the thread is fetching the next batch, splitting
            // a single paste into fragments.  Retry once with 1 ms to let
            // the thread catch up before declaring the key as solo.
            if key_text_payload(&key).is_some()
                && (event::poll(Duration::ZERO)? || event::poll(Duration::from_millis(1))?)
            {
                let (keys, leftover) = drain_key_burst(key)?;
                let text: String = keys.iter().filter_map(key_text_payload).collect();
                if burst_looks_like_paste(&text) {
                    app.paste_str(text);
                } else {
                    // Human-plausible burst (fast typing): keep exact key
                    // semantics, including a trailing Enter meaning submit.
                    for key in keys {
                        handle_key(app, key);
                    }
                }
                if let Some(event) = leftover {
                    dispatch_terminal_event(app, terminal, event)?;
                }
            } else {
                handle_key(app, key);
            }
            Ok(true)
        }
        Event::Key(_) => Ok(false),
        Event::Paste(text) => {
            app.paste_str(text);
            Ok(true)
        }
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(),
                MouseEventKind::ScrollDown => app.scroll_down(),
                MouseEventKind::Down(MouseButton::Left) => {
                    app.selection_begin(mouse.column, mouse.row);
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.selection_update(mouse.column, mouse.row);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(text) = app.selection_finish() {
                        crate::clipboard::copy(&text);
                        // The clipboard helper (clip.exe / pbcopy / xclip)
                        // is a child process that can reset the console
                        // mode on Windows; re-assert mouse capture.
                        let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
                        app.status = tr_with(
                            app.lang,
                            TextId::CopiedSelection,
                            &[("count", &text.chars().count().to_string())],
                        );
                    }
                }
                _ => {}
            }
            Ok(true)
        }
        Event::Resize(..) => Ok(true),
        _ => Ok(false),
    }
}

/// The character a key event would type, for paste-burst reconstruction.
/// `None` for shortcuts and navigation keys (they end a burst).
fn key_text_payload(key: &KeyEvent) -> Option<char> {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(ch),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// A zero-delay key burst no human could produce. 12 keys inside one poll
/// window ≈ 300 keys/sec sustained; the fastest typists burst under half of
/// that, while a console paste floods the queue instantly.
const PASTE_BURST_MIN_CHARS: usize = 12;

/// Decide whether a gathered key burst is a paste.
///
/// A multi-line burst (interior newline) is always a paste — a human typing
/// Enter would have ended the input instead of continuing.  A very long
/// instant burst without newlines is also a paste.  Crucially, **a trailing
/// newline with non-empty content** ("hello\n") is treated as paste too:
/// on Windows, crossterm's console event source does not deliver
/// `Event::Paste`, so every paste hits this burst path, and many clipboard
/// texts carry a trailing newline.  Without this rule a short trailing-`\n`
/// paste would replay keys → Enter triggers `submit()`, starting a
/// conversation or truncating the pasted content.
fn burst_looks_like_paste(text: &str) -> bool {
    let interior = text.trim_end_matches('\n');
    // Multi-line content (interior newline) → paste
    if interior.contains('\n') {
        return true;
    }
    // Implausibly long instant burst → paste
    if text.chars().count() >= PASTE_BURST_MIN_CHARS {
        return true;
    }
    // Trailing newline with non-empty content → paste
    // ("x\n" cannot be a fast typist's "x⏎" because a zero-delay burst
    //  of >1 textual keys is always a paste on modern hardware.)
    text.ends_with('\n') && !interior.is_empty()
}

/// Drain the immediately-available (or near-immediately-available) textual
/// key events following `first`.  Uses a 1 ms poll inside so that on
/// Windows, where crossterm's background reader thread may enqueue events
/// in batches, we don't return prematurely and split a single paste burst
/// into fragments (which would cause one fragment to be replayed as keys
/// and another to create a folded paste block).
fn drain_key_burst(first: KeyEvent) -> Result<(Vec<KeyEvent>, Option<Event>)> {
    let mut keys = vec![first];
    let mut leftover = None;
    while event::poll(Duration::from_millis(1))? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key_text_payload(&key).is_some() {
                    keys.push(key);
                } else {
                    leftover = Some(Event::Key(key));
                    break;
                }
            }
            // Interleaved key releases (Windows console reports them).
            Event::Key(_) => {}
            other => {
                leftover = Some(other);
                break;
            }
        }
    }
    Ok((keys, leftover))
}

/// Shift+Tab, which most terminals deliver as `BackTab` but some as `Tab` with
/// the Shift modifier. Cycles the permission mode.
fn is_shift_tab(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::BackTab)
        || (matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Any key other than Ctrl+C disarms the "press again to quit" guard.
    let is_ctrl_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        app.clear_ctrl_c_guard();
    }
    // Any key other than Shift+Tab disarms the pending-Yolo confirm.
    if !is_shift_tab(&key) {
        app.clear_yolo_arm();
    }

    // The `/resume` modal is a full-screen overlay: it owns all keys while open.
    if app.resume_picker_open() {
        match key.code {
            KeyCode::Up => app.resume_picker_up(),
            KeyCode::Down => app.resume_picker_down(),
            KeyCode::Enter => app.resume_picker_accept(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.resume_picker_cancel(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.resume_picker_cancel();
            }
            _ => {}
        }
        return;
    }

    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.handle_ctrl_c();
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => app.approve_pending_tool(),
            KeyCode::Char('a') | KeyCode::Char('A') => app.approve_pending_tool_for_session(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.deny_pending_tool(),
            KeyCode::Up => app.approval_focus_up(),
            KeyCode::Down => app.approval_focus_down(),
            KeyCode::Enter => app.execute_focused_approval(),
            KeyCode::PageUp => app.scroll_approval_up(),
            KeyCode::PageDown => app.scroll_approval_down(),
            KeyCode::Home => app.scroll_approval_to_top(),
            _ => {}
        }
        return;
    }

    // Completion menu takes over navigation/accept keys while open; plain
    // characters and backspace fall through so typing keeps filtering.
    if app.completion_open() {
        match key.code {
            KeyCode::Up => {
                app.completion_up();
                return;
            }
            KeyCode::Down => {
                app.completion_down();
                return;
            }
            // A bare Tab accepts the completion, but Shift+Tab (delivered as
            // Tab+SHIFT on some terminals) must fall through to the
            // permission-mode cycle below rather than be swallowed here — so it
            // behaves the same as on terminals that send BackTab.
            KeyCode::Tab if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let _ = app.accept_completion();
                return;
            }
            KeyCode::Enter => {
                if app.accept_completion() {
                    app.submit();
                }
                return;
            }
            _ => {}
        }
    }

    // Shift+Tab cycles the permission mode (default → accept-edits → auto → yolo).
    if is_shift_tab(&key) {
        // Dismiss the completion popup if open — cycling the mode is not a
        // completion action, and leaving the menu up over the input misleads.
        app.close_completion();
        app.cycle_permission_mode();
        return;
    }

    match key.code {
        KeyCode::Esc => app.handle_escape(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.handle_ctrl_c(),
        // Ctrl+V on Windows in raw mode is NOT intercepted by the terminal,
        // so it would fall through to push_char('v') and corrupt the input.
        // Ignore it — the terminal handles paste via its own mechanism
        // (right-click / menu) and injects characters directly.
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => app.push_newline(),
        KeyCode::Enter => app.submit(),
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => app.push_newline(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => app.history_prev(),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => app.history_next(),
        // Readline-style editing.
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.delete_word_back();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.kill_to_line_start();
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.kill_to_line_end();
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_home(),
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_end(),
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => app.delete_word_back(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward(),
        // Word-wise cursor movement (Ctrl/Alt + ←→).
        KeyCode::Left
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.word_left();
        }
        KeyCode::Right
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.word_right();
        }
        KeyCode::Left => app.cursor_left(),
        KeyCode::Right => app.cursor_right(),
        KeyCode::Home => app.cursor_home(),
        KeyCode::End => app.cursor_end(),
        // Shift+↑/↓ (and PageUp/PageDown) scroll the transcript — laptop
        // friendly since the mouse is left for native selection/copy.
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => app.scroll_up(),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => app.scroll_down(),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        // Plain ↑↓ serve the composer (cursor between lines, else history).
        KeyCode::Up => app.on_up(),
        KeyCode::Down => app.on_down(),
        KeyCode::Char(value) => app.push_char(value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_burst_paste_detection_rules() {
        // Interior newline = pasted multi-line content, however short.
        assert!(burst_looks_like_paste("ab\ncd"));
        assert!(burst_looks_like_paste("a\nb\n"));
        // Implausibly long instant burst = paste even without newlines.
        assert!(burst_looks_like_paste("cargo test --workspace"));
        // Fast human typing: short text without trailing newline is not a
        // paste (it will replay as normal typed chars).
        assert!(!burst_looks_like_paste("hi"));
        // ★ Trailing newline with content = paste on Windows
        //   ("hi\n" / "好的\n" arriving as a zero-delay burst can only be a
        //    paste; a human would never type >1 textual key in zero delay.)
        assert!(burst_looks_like_paste("hi\n"));
        assert!(burst_looks_like_paste("好的\n"));
    }

    #[test]
    fn key_text_payload_maps_typed_keys_only() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        assert_eq!(key_text_payload(&plain(KeyCode::Char('中'))), Some('中'));
        assert_eq!(key_text_payload(&plain(KeyCode::Enter)), Some('\n'));
        assert_eq!(key_text_payload(&plain(KeyCode::Tab)), Some('\t'));
        // Shift-typed uppercase is still text.
        assert_eq!(
            key_text_payload(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
        // Shortcuts and navigation end a burst.
        assert_eq!(
            key_text_payload(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(key_text_payload(&plain(KeyCode::Up)), None);
        assert_eq!(key_text_payload(&plain(KeyCode::Backspace)), None);
    }
}
