//! Terminal event routing and key handling: turns crossterm events into
//! [`App`] state mutations, including the Windows paste-burst heuristic.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
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
            if let Some(text) = handle_mouse(app, mouse) {
                crate::clipboard::copy(&text);
                // The clipboard helper (clip.exe / pbcopy / xclip) is a child
                // process that can reset the console mode on Windows;
                // re-assert mouse capture.
                let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
                app.status = tr_with(
                    app.lang,
                    TextId::CopiedSelection,
                    &[("count", &text.chars().count().to_string())],
                );
            }
            Ok(true)
        }
        Event::Resize(..) => Ok(true),
        _ => Ok(false),
    }
}

/// Route one mouse event; returns the text a finished drag selected, for the
/// caller to put on the clipboard.
fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Option<String> {
    // The `/resume` overlay covers the transcript and the composer, but the
    // last frame's transcript snapshot outlives it: a drag across the overlay
    // copied text from the hidden transcript, a click folded a block nobody
    // could see, and the wheel scrolled it.
    if app.resume_picker_open() {
        return None;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => app.scroll_up(),
        MouseEventKind::ScrollDown => app.scroll_down(),
        MouseEventKind::Down(MouseButton::Left) => app.mouse_press(mouse.column, mouse.row),
        MouseEventKind::Drag(MouseButton::Left) => app.selection_update(mouse.column, mouse.row),
        MouseEventKind::Up(MouseButton::Left) => {
            return app.mouse_release(mouse.column, mouse.row);
        }
        _ => {}
    }
    None
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
    // Same for the Esc guard: only a second consecutive Esc quits.
    if key.code != KeyCode::Esc {
        app.clear_esc_guard();
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
        // A decision fires only on an UNMODIFIED key (Shift allowed, for `Y`/`N`).
        // The approval keys `y`/`a`/`n` collide with composer chords —
        // Ctrl+A is cursor-home, Alt+Enter is newline, Ctrl+Y approves — and a
        // reflexive chord must never be read as "approve for the session"
        // against a security prompt. Navigation/scrolling stay live under any
        // modifier; they resolve nothing.
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        // A decision needs a panel the user has actually seen AND had a moment
        // to register: drawn at least once (`approval_armed`), and on screen for
        // `APPROVAL_ARM_DELAY`. A key already in flight when the prompt pops —
        // the `y` in a steering message being typed as a tool call parks — lands
        // inside that window and is swallowed rather than resolving the prompt.
        let ready = app.approval_armed
            && app
                .approval_shown_at
                .is_some_and(|at| at.elapsed() >= crate::app::APPROVAL_ARM_DELAY);
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.handle_ctrl_c();
            }
            // Swallowed until the panel is ready; navigation and scrolling stay
            // live, because neither of them resolves anything.
            KeyCode::Char('y' | 'Y' | 'a' | 'A' | 'n' | 'N') | KeyCode::Enter | KeyCode::Esc
                if plain && !ready => {}
            KeyCode::Char('y' | 'Y') if plain => app.approve_pending_tool(),
            KeyCode::Char('a' | 'A') if plain => app.approve_pending_tool_for_session(),
            KeyCode::Char('n' | 'N') if plain => app.deny_pending_tool(),
            KeyCode::Esc if plain => app.deny_pending_tool(),
            KeyCode::Up => app.approval_focus_up(),
            KeyCode::Down => app.approval_focus_down(),
            KeyCode::Enter if plain => app.execute_focused_approval(),
            KeyCode::PageUp => app.scroll_approval_up(),
            KeyCode::PageDown => app.scroll_approval_down(),
            KeyCode::Home => app.scroll_approval_to_top(),
            // Pairs with Home, and with the overflow indicator that now tells
            // the user there is more below.
            KeyCode::End => app.scroll_approval_to_bottom(),
            _ => {}
        }
        return;
    }

    // Ctrl+R opens the history search, or steps it to the next older match.
    if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        app.history_search_start();
        return;
    }
    // While the search is open it owns the typing keys; anything else keeps
    // the shown match and goes on to do its usual job.
    if app.history_search_open() {
        match key.code {
            KeyCode::Esc => {
                app.history_search_cancel();
                return;
            }
            KeyCode::Enter => {
                app.history_search_accept();
                return;
            }
            KeyCode::Backspace => {
                app.history_search_backspace();
                return;
            }
            KeyCode::Up => {
                app.history_search_older();
                return;
            }
            KeyCode::Char(value) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                app.history_search_push(value);
                return;
            }
            _ => app.history_search_accept(),
        }
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
            // Plain Enter accepts+submits; Alt+Enter is a newline, so let it
            // fall through to the composer handler below instead of submitting.
            KeyCode::Enter if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
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
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Deliberately not a no-op. A terminal that handles the paste itself
            // never sends us this key — we get an `Event::Paste` instead — so a
            // Ctrl+V that *does* arrive is a terminal asking us to read the
            // clipboard, and an image is the only thing bracketed paste cannot
            // carry. With no image on the clipboard this does nothing, which
            // keeps the key transparent for terminals that forward it for text.
            app.paste_clipboard_image();
        }
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
        // Only an unmodified (or Shift-only) character is composer input. An
        // unbound chord — Ctrl+L, Ctrl+R, Alt+d — otherwise fell through here
        // and inserted its bare letter into the prompt.
        KeyCode::Char(value) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
            app.push_char(value)
        }
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

    /// A decision key that was already in the queue when the approval arrived
    /// must not resolve it.
    ///
    /// The run loop applies runtime events, may skip the frame, then reads
    /// whatever key is waiting — so a `y` typed as the first letter of a
    /// steering message could resolve a boundary prompt that had never been
    /// drawn. The user's evidence for this was their own composer text still
    /// sitting unsent while the write boundary had widened.
    #[tokio::test]
    async fn a_key_queued_before_the_panel_was_drawn_cannot_resolve_it() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut app = App::new();
        app.park_approval(deep_code_agent::ApprovalRequest {
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "mock_echo".to_string(),
            description: "echo".to_string(),
            arguments: serde_json::json!({}),
            risk_level: deep_code_agent::RiskLevel::Low,
            requires_sandbox: false,
            read_only: true,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        });

        // Nothing has been drawn: every decision key is inert.
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Esc,
        ] {
            handle_key(&mut app, plain(code));
            assert!(
                app.pending_approval.is_some(),
                "{code:?} resolved a panel the user has not seen"
            );
        }

        // Scrolling stays live — it cannot resolve anything, and a user who
        // needs to read past the fold before the panel arms must be able to.
        handle_key(&mut app, plain(KeyCode::PageDown));
        assert!(app.pending_approval.is_some());
        assert_eq!(app.approval_scroll_offset, 3, "PageDown still scrolls");
        handle_key(&mut app, plain(KeyCode::End));
        assert_eq!(
            app.approval_scroll_offset,
            usize::MAX,
            "End jumps to the bottom (the render layer clamps it to the last line)"
        );
        handle_key(&mut app, plain(KeyCode::Home));
        assert_eq!(app.approval_scroll_offset, 0);

        // Drawing the panel is what arms it (see `render_approval_panel`), and a
        // decision is accepted only after it has been on screen for the arm
        // delay — simulate that here so the panel is fully ready.
        app.approval_armed = true;
        app.approval_shown_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        handle_key(&mut app, plain(KeyCode::Char('y')));
        assert!(
            app.pending_approval.is_none(),
            "once the panel has been drawn and settled, y must approve as before"
        );
    }

    /// A modified key never resolves an armed approval panel. Ctrl+A is the
    /// composer's cursor-home, Alt+Enter its newline, Ctrl+Y a stray chord — a
    /// reflex on any of them used to record "approve for the session" / approve
    /// against a security prompt.
    #[tokio::test]
    async fn a_modified_key_never_resolves_an_armed_approval() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let park = |app: &mut App| {
            app.park_approval(deep_code_agent::ApprovalRequest {
                network: false,
                call_id: "call_1".to_string(),
                tool_name: "mock_echo".to_string(),
                description: "echo".to_string(),
                arguments: serde_json::json!({}),
                risk_level: deep_code_agent::RiskLevel::Low,
                requires_sandbox: false,
                read_only: true,
                matched_rule: None,
                justification: None,
                resolved_target: None,
                preview: None,
                safety_notes: Vec::new(),
            });
            app.approval_armed = true;
            // Simulate the panel having been on screen past the arm delay, so
            // this test isolates the modifier gate, not the settle timer.
            app.approval_shown_at =
                Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        };

        for (code, modifiers) in [
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('y'), KeyModifiers::CONTROL),
            (KeyCode::Char('n'), KeyModifiers::ALT),
            (KeyCode::Char('a'), KeyModifiers::SUPER),
            (KeyCode::Enter, KeyModifiers::ALT),
            (KeyCode::Esc, KeyModifiers::CONTROL),
        ] {
            let mut app = App::new();
            park(&mut app);
            handle_key(&mut app, KeyEvent::new(code, modifiers));
            assert!(
                app.pending_approval.is_some(),
                "{code:?}+{modifiers:?} resolved a security prompt"
            );
        }

        // The unmodified (and Shift-only) keys still decide.
        let mut app = App::new();
        park(&mut app);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(app.pending_approval.is_none(), "plain `a` still approves");
    }

    /// A decision key is swallowed while the panel is armed but has NOT been on
    /// screen for the arm delay yet — the burst-arrives-as-the-panel-pops case.
    #[test]
    fn a_decision_key_is_swallowed_until_the_panel_settles() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.park_approval(deep_code_agent::ApprovalRequest {
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "mock_echo".to_string(),
            description: "echo".to_string(),
            arguments: serde_json::json!({}),
            risk_level: deep_code_agent::RiskLevel::Low,
            requires_sandbox: false,
            read_only: true,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        });
        // Drawn, but just now — inside the arm delay.
        app.approval_armed = true;
        app.approval_shown_at = Some(std::time::Instant::now());
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert!(
            app.pending_approval.is_some(),
            "a key within the arm delay must not resolve the panel"
        );
    }

    /// End-to-end steering guard at the layer that actually shipped broken:
    /// real key presses routed through `handle_key` while a turn is streaming.
    /// The previous tests all assigned `app.input` directly, so they passed
    /// while typing mid-stream was silently discarded by `push_char`.
    #[test]
    fn typing_mid_stream_reaches_composer_and_enter_queues_it() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut app = App::new();
        app.is_streaming = true;

        for ch in "skip tests".chars() {
            handle_key(&mut app, plain(KeyCode::Char(ch)));
        }
        assert_eq!(
            app.input, "skip tests",
            "keys typed mid-stream must land in the composer"
        );

        handle_key(&mut app, plain(KeyCode::Enter));
        assert_eq!(
            app.steering_queue,
            vec!["skip tests".to_string()],
            "Enter mid-stream queues the prompt as a follow-up"
        );
        assert!(app.input.is_empty(), "composer clears once queued");
        assert!(app.is_streaming, "queueing must not end the current turn");
    }

    /// Recall over an open menu, through real keys: `/c` opens the command
    /// menu, Ctrl+P (which the menu does not take, unlike ↑) recalls a prompt,
    /// Enter sends it. The menu used to stay open over the recalled prompt, and
    /// Enter applied it — ran `/clear`, its first entry — so what was recalled
    /// was never sent. Mid-stream, so Enter queues it instead of starting a turn.
    #[test]
    fn enter_sends_a_prompt_recalled_over_an_open_menu() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut app = App::new();
        app.remember_prompt("rerun the flaky test");
        app.is_streaming = true;
        for ch in "/c".chars() {
            handle_key(&mut app, plain(KeyCode::Char(ch)));
        }
        assert!(app.completion_open(), "sanity: /c opens the command menu");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.input, "rerun the flaky test");
        handle_key(&mut app, plain(KeyCode::Enter));

        assert_eq!(
            app.steering_queue,
            vec!["rerun the flaky test".to_string()],
            "Enter sent the recalled prompt, not the menu's `/clear`"
        );
    }

    /// ↑ walks back past a prompt that was a slash command. Recalling it closes
    /// the menu rather than recomputing one for it: a recalled `/help` would
    /// open the command menu, the menu takes ↑, and the walk would stop there.
    #[test]
    fn up_walks_past_a_recalled_slash_command() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut app = App::new();
        app.remember_prompt("first");
        app.remember_prompt("/help");
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(app.input, "/help");
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(app.input, "first", "the menu took ↑");
    }

    // -----------------------------------------------------------------------
    // Clicks on the composer, through the same routing the terminal's events
    // take, on a frame actually drawn.
    // -----------------------------------------------------------------------

    use deep_code_agent::i18n::Lang;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A press and a release on one cell; returns what a drag would have copied.
    fn click(app: &mut App, col: u16, row: u16) -> Option<String> {
        handle_mouse(
            app,
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
        );
        handle_mouse(app, mouse(MouseEventKind::Up(MouseButton::Left), col, row))
    }

    fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) -> Option<String> {
        handle_mouse(
            app,
            mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
        );
        handle_mouse(
            app,
            mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
        );
        handle_mouse(
            app,
            mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
        )
    }

    fn en_app(draft: &str) -> App {
        let mut app = App::new();
        app.lang = Lang::En;
        app.input = draft.to_string();
        app.input_cursor = app.input.chars().count();
        app
    }

    /// Draw the whole app; the screen as rows, and the caret the terminal was
    /// handed — the observable a person checks after clicking.
    fn draw(terminal: &mut Terminal<TestBackend>, app: &mut App) -> (Vec<String>, (u16, u16)) {
        terminal
            .draw(|frame| super::super::render::render(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|col| buffer[(col, row)].symbol())
                    .collect()
            })
            .collect();
        let caret = terminal.get_cursor_position().unwrap();
        (rows, (caret.x, caret.y))
    }

    /// The cell a glyph was drawn in (the left one, for a wide glyph). The
    /// markers are glyphs no chrome draws — Greek, a rare ideograph — so the
    /// search cannot be satisfied by the welcome card or the status line.
    fn cell_of(terminal: &Terminal<TestBackend>, glyph: &str) -> (u16, u16) {
        let buffer = terminal.backend().buffer();
        for row in 0..buffer.area.height {
            for col in 0..buffer.area.width {
                if buffer[(col, row)].symbol() == glyph {
                    return (col, row);
                }
            }
        }
        panic!("{glyph:?} is not on screen");
    }

    /// A click on the composer's text moves the caret to the click — and the
    /// caret is then drawn in the cell that was clicked.
    #[test]
    fn a_click_on_the_composer_puts_the_caret_there() {
        // (draft, glyph clicked, which of its cells, caret index, glyph the
        // caret must then be drawn on)
        let wrapped = format!("{}bbΞ", "a".repeat(38));
        let cases: [(&str, &str, u16, usize, &str); 5] = [
            ("alpha Ψ beta", "Ψ", 0, 6, "Ψ"),
            ("左龘右", "龘", 0, 1, "龘"),
            // The right half of a wide glyph is the position after it.
            ("左龘右", "龘", 1, 2, "右"),
            // 38 columns of text at width 40: Ξ is on the second row.
            (&wrapped, "Ξ", 0, 40, "Ξ"),
            ("first\nsecond Ω", "Ω", 0, 13, "Ω"),
        ];
        for (draft, glyph, cell, want, caret_on) in cases {
            let mut app = en_app(draft);
            let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
            draw(&mut terminal, &mut app);
            let (x, y) = cell_of(&terminal, glyph);

            assert_eq!(click(&mut app, x + cell, y), None, "a click copies nothing");
            assert_eq!(app.input_cursor, want, "{draft:?}: cell {cell} of {glyph}");
            let (_, caret) = draw(&mut terminal, &mut app);
            assert_eq!(caret, cell_of(&terminal, caret_on), "{draft:?}");
        }
    }

    /// A click beside the text: on the `› ` gutter it is the start of that row,
    /// right of a line it is the end of that line.
    #[test]
    fn a_click_beside_the_composer_text_takes_the_end_of_its_row() {
        let mut app = en_app(&format!("{}bbΞ", "a".repeat(38)));
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        draw(&mut terminal, &mut app);
        let (_, second_row) = cell_of(&terminal, "Ξ");
        click(&mut app, 0, second_row);
        assert_eq!(app.input_cursor, 38, "the gutter of the second row");
        let (_, caret) = draw(&mut terminal, &mut app);
        assert_eq!(caret, (2, second_row));

        let mut app = en_app("Ψ short\nsecond line");
        draw(&mut terminal, &mut app);
        let (_, first_row) = cell_of(&terminal, "Ψ");
        click(&mut app, 30, first_row);
        assert_eq!(app.input_cursor, 7, "right of the first line: its end");
        let (_, caret) = draw(&mut terminal, &mut app);
        assert_eq!(caret, (2 + 7, first_row));

        // The rules above and below the text are not text.
        app.input_cursor = 3;
        click(&mut app, 4, first_row - 1);
        click(&mut app, 4, first_row + 2);
        assert_eq!(app.input_cursor, 3, "a click on the composer's border");
    }

    /// A press on the composer ends the transcript's gesture too. Left in place,
    /// the selection a previous drag made was still there when this click's
    /// release arrived, and the release copied it again.
    #[test]
    fn a_click_on_the_composer_drops_a_transcript_selection() {
        let mut app = en_app("Ξ draft");
        app.history.push(crate::history::HistoryCell::user(
            "ΨΨ some transcript text ΩΩ",
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        draw(&mut terminal, &mut app);
        let copied = drag(&mut app, cell_of(&terminal, "Ψ"), cell_of(&terminal, "Ω"));
        assert!(
            copied.is_some(),
            "sanity: the drag selected transcript text"
        );

        let (x, y) = cell_of(&terminal, "Ξ");
        assert_eq!(
            click(&mut app, x, y),
            None,
            "the old selection copied again"
        );
        assert!(app.selection.is_none());
        assert_eq!(app.input_cursor, 0);
    }

    /// In a draft taller than the composer, a click on a row of the window must
    /// not scroll it: the text under the pointer stays where it was. The window
    /// used to be re-derived from the caret, pinning it to the bottom row, so
    /// the clicked row slid down out from under the click.
    #[test]
    fn a_click_in_a_scrolled_draft_does_not_move_the_text_under_the_pointer() {
        // Twelve rows of 38, one letter each, and a part-filled last row.
        let letters = "ΑΒΓΔΕΖΗΘΙΚΛΜ";
        let mut draft: String = letters
            .chars()
            .map(|ch| ch.to_string().repeat(38))
            .collect();
        draft.push('Ψ');
        let mut app = en_app(&draft);
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        let (before, _) = draw(&mut terminal, &mut app);
        // Six rows show, the caret's (Ψ) last: Θ is the top one.
        let (x, y) = cell_of(&terminal, "Θ");
        assert!(
            !before.iter().any(|row| row.contains('Η')),
            "the row above the window is hidden"
        );

        click(&mut app, x + 5, y);
        assert_eq!(app.input_cursor, 7 * 38 + 5);
        let (after, caret) = draw(&mut terminal, &mut app);
        assert_eq!(after, before, "the window moved under the click");
        assert_eq!(caret, (x + 5, y));
    }

    /// A wide glyph that does not fit the end of a row starts the next one,
    /// leaving a blank cell behind — an everyday shape in Chinese text. A click
    /// on that glyph is the position before it, and the caret has to be drawn
    /// on it: it used to be drawn in the blank cell at the end of the row
    /// above, and when that row was scrolled out of the window, the window
    /// followed it up.
    #[test]
    fn a_click_on_a_glyph_that_wrapped_for_lack_of_room_draws_the_caret_on_it() {
        // 37 of the 38 text columns, then a wide glyph that needs two.
        let mut app = en_app(&format!("{}龘Ξ", "a".repeat(37)));
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        draw(&mut terminal, &mut app);
        let glyph = cell_of(&terminal, "龘");
        click(&mut app, glyph.0, glyph.1);
        assert_eq!(app.input_cursor, 37);
        let (_, caret) = draw(&mut terminal, &mut app);
        assert_eq!(caret, glyph, "the caret is drawn on the glyph clicked");

        // Every row opens with such a glyph; the window shows the last six.
        let wide = "一二三四五六七八九十百千";
        let mut draft: String = wide
            .chars()
            .map(|ch| format!("{ch}{}", "a".repeat(35)))
            .collect();
        draft.push_str("\nΨ");
        let mut app = en_app(&draft);
        let (before, _) = draw(&mut terminal, &mut app);
        let top = cell_of(&terminal, "八");
        assert!(
            !before.iter().any(|row| row.contains('七')),
            "八 opens the window"
        );
        click(&mut app, top.0, top.1);
        assert_eq!(app.input_cursor, 7 * 36);
        let (after, caret) = draw(&mut terminal, &mut app);
        assert_eq!(after, before, "the window moved under the click");
        assert_eq!(caret, top);
    }

    /// The completion menu's frame lays the composer out on its own path, and
    /// a click there must land the same way.
    #[test]
    fn a_click_lands_in_the_composer_while_the_completion_menu_is_open() {
        let mut app = en_app("");
        for ch in "/re".chars() {
            app.push_char(ch);
        }
        assert!(app.completion_open(), "sanity: `/re` opens the menu");
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        draw(&mut terminal, &mut app);
        let composer = app.composer.clone().expect("the composer takes clicks");
        click(&mut app, composer.text_x + 1, composer.y);
        assert_eq!(app.input_cursor, 1, "between / and r");
        let (_, caret) = draw(&mut terminal, &mut app);
        assert_eq!(caret, (composer.text_x + 1, composer.y));
    }

    /// A click on a chip lands beside it, never in it. An image chip and a
    /// paste chip are found by their exact text, so a character typed inside
    /// one drops the picture, or sends the placeholder instead of the paste.
    #[test]
    fn a_click_on_a_chip_lands_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3],
        )
        .unwrap();
        let mut app = en_app("");
        for ch in "see ".chars() {
            app.push_char(ch);
        }
        assert!(app.attach_image(png));
        for ch in " and ".chars() {
            app.push_char(ch);
        }
        app.paste_str("one\ntwo".to_string());
        let image = app.attached_images[0].chip.clone();
        let paste = app.pasted_blocks[0].0.clone();
        let image_start = 4;
        let image_end = image_start + image.chars().count();
        let paste_start = image_end + 5;
        let paste_end = paste_start + paste.chars().count();
        assert_eq!(app.input, format!("see {image} and {paste}"));

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        // ASCII on one row: the char at index `i` is drawn at `text_x + i`.
        let at = |app: &App, index: usize| {
            let composer = app.composer.as_ref().expect("the composer takes clicks");
            (composer.text_x + index as u16, composer.y)
        };
        for (index, want) in [
            (image_start + 1, image_start),
            (image_end - 1, image_end),
            (paste_start + 2, paste_start),
            (paste_end - 3, paste_end),
        ] {
            draw(&mut terminal, &mut app);
            let (x, y) = at(&app, index);
            click(&mut app, x, y);
            assert_eq!(app.input_cursor, want, "click on index {index}");
        }

        // Typing where the click left the caret keeps both chips whole.
        app.input_cursor = image_end;
        app.push_char('!');
        assert!(app.input.contains(&image) && app.input.contains(&paste));
        assert_eq!(app.attached_images.len(), 1, "the image is still attached");
        assert!(app.expand_pasted(&app.input).contains("one\ntwo"));
    }

    /// Under an approval panel the composer is drawn but takes no input — its
    /// keys are y/a/n — so it takes no click either.
    #[test]
    fn the_composer_takes_no_click_under_an_approval_panel() {
        let mut app = en_app("draft text");
        app.park_approval(deep_code_agent::ApprovalRequest {
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "mock_echo".to_string(),
            description: "echo".to_string(),
            arguments: serde_json::json!({}),
            risk_level: deep_code_agent::RiskLevel::Low,
            requires_sandbox: false,
            read_only: true,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        let (rows, _) = draw(&mut terminal, &mut app);
        let y = rows
            .iter()
            .position(|row| row.contains("draft text"))
            .expect("the composer is still drawn") as u16;
        // Checked after every click: the last cell is right of the text, where
        // a click that did land would put the caret back at the end.
        for x in 0..60 {
            click(&mut app, x, y);
            assert_eq!(
                app.input_cursor, 10,
                "a click at column {x} moved the caret"
            );
        }
        assert!(app.pending_approval.is_some());
    }

    /// A frame drawn before the latest edit says nothing about where the text
    /// is now (redraws are rate-limited, so a click can arrive in between).
    /// Such a click is dropped; the next frame takes it.
    #[test]
    fn a_click_on_a_frame_older_than_the_draft_is_dropped() {
        let mut app = en_app("Ψabc");
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        draw(&mut terminal, &mut app);
        let (x, y) = cell_of(&terminal, "Ψ");

        app.push_char('d');
        click(&mut app, x, y);
        assert_eq!(
            app.input_cursor, 5,
            "the click was on a frame without the d"
        );

        draw(&mut terminal, &mut app);
        click(&mut app, x, y);
        assert_eq!(app.input_cursor, 0);
    }

    /// The `/resume` overlay covers everything, but the transcript's snapshot
    /// from the last frame under it survived: the same drag that selects and
    /// copies text with the overlay closed copied the hidden transcript with it
    /// open, and the wheel scrolled what nobody could see.
    #[test]
    fn the_mouse_does_nothing_under_the_resume_picker() {
        use deep_code_agent::{SessionEntry, SessionRecord};

        let mut app = en_app("");
        app.history
            .push(crate::history::HistoryCell::user("ΨΨ hidden transcript ΩΩ"));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        draw(&mut terminal, &mut app);
        let from = cell_of(&terminal, "Ψ");
        let to = cell_of(&terminal, "Ω");
        let copied = drag(&mut app, from, to);
        assert!(
            copied
                .as_deref()
                .is_some_and(|text| text.contains("hidden")),
            "sanity: with the overlay closed this drag copies {copied:?}"
        );

        let mut record = SessionRecord::new(std::path::PathBuf::from("/tmp/ws"), "system");
        record.entries = vec![
            std::sync::Arc::new(SessionEntry::system("system")),
            std::sync::Arc::new(SessionEntry::user("earlier")),
        ];
        // What `open_resume_picker` does on the way in.
        app.clear_selection();
        app.resume_picker = Some(crate::app::ResumePicker {
            sessions: vec![record],
            selected: 0,
        });
        draw(&mut terminal, &mut app);
        assert_eq!(
            drag(&mut app, from, to),
            None,
            "copied the hidden transcript"
        );
        assert!(app.selection.is_none());
        let scrolled = app.scroll_offset;
        handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, from.0, from.1));
        assert_eq!(
            app.scroll_offset, scrolled,
            "scrolled the hidden transcript"
        );
    }

    /// Ctrl+R opens the search over the draft; Esc closes it with the draft
    /// back; a key the search does not own keeps the match and still does its
    /// own job.
    #[test]
    fn ctrl_r_opens_the_history_search_and_other_keys_leave_it() {
        let mut app = en_app("draft");
        app.prompt_history.push(crate::app::HistoryPrompt {
            text: "earlier prompt".to_string(),
            images: Vec::new(),
        });
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        assert!(app.history_search_open());
        assert_eq!(app.input, "earlier prompt");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!app.history_search_open());
        assert_eq!(app.input, "draft");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert!(!app.history_search_open(), "Ctrl+A leaves the search");
        assert_eq!(app.input, "earlier prompt");
        assert_eq!(app.input_cursor, 0, "and still moves to the line start");
    }
}
