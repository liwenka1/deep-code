use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style, Stylize};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Widget};
use ratatui::{Terminal, backend::CrosstermBackend};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, LaunchConfig, TranscriptSnapshot};
use crate::history::HistoryCell;
use crate::markdown::render_markdown;

/// Redraws are coalesced to at most ~30fps; streaming deltas mark the UI
/// dirty instead of forcing a frame each.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// While streaming, repaint at least this often so the activity indicator
/// animates through a long time-to-first-token wait.
const STREAM_TICK_INTERVAL: Duration = Duration::from_millis(120);
/// Max completion rows shown at once; the list windows around the selection so
/// wrapping past the top/bottom keeps the highlighted item on screen.
const COMPLETION_VISIBLE_ROWS: usize = 8;
type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(config: LaunchConfig) -> Result<()> {
    // The alternate-screen TUI owns the terminal; any stray write to stderr
    // (LSP "server unavailable" notices, persistence warnings, panics) would
    // paint raw text over the rendered buffer. Send stderr to a log file so it
    // can never corrupt the screen.
    redirect_stderr_to_log();
    let mut terminal = setup_terminal()?;
    let mut app = App::launch(config);
    let result = run_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result?;
    app.shutdown_runtime().await;
    Ok(())
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
            terminal.draw(|frame| render(frame, app))?;
            last_draw = Some(Instant::now());
            needs_redraw = false;
        }

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(app, key);
                    needs_redraw = true;
                }
                Event::Paste(text) => {
                    app.paste_str(text);
                    needs_redraw = true;
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
                                app.status =
                                    format!("已复制选中文本 ({} 字)", text.chars().count());
                            }
                        }
                        _ => {}
                    }
                    needs_redraw = true;
                }
                Event::Resize(..) => needs_redraw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Any key other than Ctrl+C disarms the "press again to quit" guard.
    let is_ctrl_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        app.clear_ctrl_c_guard();
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
            KeyCode::Tab => {
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

    match key.code {
        KeyCode::Esc => app.handle_escape(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.handle_ctrl_c(),
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

/// Soft upper bound for composer visible rows before scrolling.
pub(crate) const COMPOSER_MAX_VISIBLE_ROWS: usize = 6;

fn render(frame: &mut Frame<'_>, app: &mut App) {
    if let Some(picker) = &app.resume_picker {
        render_resume_picker(frame, picker);
        return;
    }

    let inner_width = frame.area().width.saturating_sub(2).max(1);
    // Compute layout once — height and rendering share the same result.
    let layout = layout_input(
        &app.input,
        app.input_cursor,
        inner_width as usize,
        COMPOSER_MAX_VISIBLE_ROWS,
    );
    let visual_rows = layout.total_rows.clamp(1, COMPOSER_MAX_VISIBLE_ROWS);
    let input_height = Constraint::Length(visual_rows as u16 + 2);

    let snapshot: TranscriptSnapshot = if app.pending_approval.is_some() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(6),
                input_height,
                Constraint::Length(1),
            ])
            .split(frame.area());
        let snap = render_messages(frame, app, chunks[0]);
        render_approval_panel(frame, app, chunks[1]);
        render_input_from_layout(frame, app, &layout, chunks[2]);
        render_status(frame, app, chunks[3]);
        snap
    } else if let Some(menu) = &app.completion {
        let menu_height = (menu.items.len() as u16).min(COMPLETION_VISIBLE_ROWS as u16) + 2;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(menu_height),
                input_height,
                Constraint::Length(1),
            ])
            .split(frame.area());
        let snap = render_messages(frame, app, chunks[0]);
        render_completion_menu(frame, menu, chunks[1]);
        render_input_from_layout(frame, app, &layout, chunks[2]);
        render_status(frame, app, chunks[3]);
        snap
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), input_height, Constraint::Length(1)])
            .split(frame.area());
        let snap = render_messages(frame, app, chunks[0]);
        render_input_from_layout(frame, app, &layout, chunks[1]);
        render_status(frame, app, chunks[2]);
        snap
    };
    app.set_transcript_snapshot(snapshot);
}

/// In-app `/resume` overlay. Mirrors the standalone startup picker's minimal
/// look, but draws inside the live alt-screen (a full-area `Clear` wipes the
/// transcript beneath) so opening/closing it never flickers.
fn render_resume_picker(frame: &mut Frame<'_>, picker: &crate::app::ResumePicker) {
    use crate::startup::{now_ms, relative_time, session_title};
    let area = frame.area();
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            "历史会话",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ])
    .block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(header, chunks[0]);

    let viewport = usize::from(chunks[1].height).max(1);
    let selected = picker.selected;
    let start = selected.saturating_sub(viewport.saturating_sub(1));
    let now = now_ms();
    let rows: Vec<Line> = picker
        .sessions
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, record)| {
            let time = relative_time(now, record.updated_at_ms);
            let title = session_title(record);
            if index == selected {
                Line::from(vec![
                    Span::styled(
                        "› ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{title}  "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(time, Style::default().fg(Color::DarkGray)),
                ])
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::raw(title),
                    Span::styled(format!("  {time}"), Style::default().fg(Color::DarkGray)),
                ])
            }
        })
        .collect();
    let list = Paragraph::new(rows).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(list, chunks[1]);

    let note = picker
        .sessions
        .first()
        .map(|record| deep_code_agent::format_sessions_storage_note(&record.workspace))
        .unwrap_or_default();
    let help = Paragraph::new(vec![
        Line::from(Span::styled(
            "↑/↓ 选择 · Enter 恢复 · n/Esc 取消",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(note, Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(help, chunks[2]);
}

fn render_completion_menu(
    frame: &mut Frame<'_>,
    menu: &crate::app::CompletionMenu,
    area: ratatui::layout::Rect,
) {
    // Window around the selection so wrapping to the last/first item keeps the
    // highlight visible instead of scrolling it off the top of the list.
    let start = menu
        .selected
        .saturating_sub(COMPLETION_VISIBLE_ROWS.saturating_sub(1));
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(COMPLETION_VISIBLE_ROWS)
        .map(|(index, (value, hint))| {
            let marker = if index == menu.selected { "▶ " } else { "  " };
            let mut spans = vec![Span::raw(marker.to_string())];
            let value_span = Span::raw(value.clone());
            if index == menu.selected {
                spans.push(value_span.bold());
            } else {
                spans.push(value_span);
            }
            if !hint.is_empty() {
                spans.push(Span::styled(
                    format!("  {hint}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let panel = Paragraph::new(lines).block(
        Block::default()
            .title("补全: ↑/↓ 选择 | Tab/Enter 确认 | Esc 关闭")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
    );
    frame.render_widget(panel, area);
}

fn render_messages(
    frame: &mut Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
) -> TranscriptSnapshot {
    // Claude-style: no transcript border/title — a 1-col left gutter and the
    // input box below provide all the structure.
    let viewport = usize::from(area.height).max(1);
    let content_width = area.width.saturating_sub(2).max(8);

    // Render the WHOLE transcript into a stable line buffer: a fixed
    // coordinate space is what lets mouse drag-selection map cleanly, and
    // bottom-anchored scroll then just windows it.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for cell in &app.history {
        lines.extend(cell_lines(cell, content_width));
    }
    let preview = app
        .active_turn
        .as_ref()
        .map(|active| active.preview_cells())
        .unwrap_or_default();
    for cell in &preview {
        lines.extend(cell_lines(cell, content_width));
    }

    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = app.scroll_offset.min(max_scroll);
    let scroll_top = max_scroll - scroll;

    let plain: Vec<String> = lines.iter().map(line_plain_text).collect();

    let paragraph = Paragraph::new(lines)
        .block(Block::default().padding(Padding::new(1, 0, 0, 0)))
        .scroll((scroll_top as u16, 0));
    frame.render_widget(paragraph, area);

    if let Some(sel) = app.selection {
        highlight_selection(frame, area, scroll_top, viewport, &plain, sel);
    }

    TranscriptSnapshot {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
        scroll_top,
        lines: plain,
    }
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Overlay reverse-video on the selected span (post-render buffer styling, so
/// it composes over whatever colours the cells already used).
fn highlight_selection(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    scroll_top: usize,
    viewport: usize,
    lines: &[String],
    selection: (crate::app::TextPos, crate::app::TextPos),
) {
    let (a, b) = selection;
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let text_x = area.x.saturating_add(1);
    let style = Style::default().add_modifier(Modifier::REVERSED);
    for line in start.0..=end.0 {
        if line < scroll_top || line >= scroll_top + viewport {
            continue;
        }
        let Some(text) = lines.get(line) else {
            continue;
        };
        let width = UnicodeWidthStr::width(text.as_str());
        let from = if line == start.0 { start.1 } else { 0 }.min(width);
        let to = if line == end.0 { end.1 } else { width }.min(width);
        if to <= from {
            continue;
        }
        let y = area.y + (line - scroll_top) as u16;
        let x = text_x.saturating_add(from as u16);
        let avail = area.x.saturating_add(area.width).saturating_sub(x);
        let w = ((to - from) as u16).min(avail);
        if w == 0 {
            continue;
        }
        frame
            .buffer_mut()
            .set_style(ratatui::layout::Rect::new(x, y, w, 1), style);
    }
}

/// Render one transcript cell, Claude-style: speakers are distinguished by a
/// marker glyph + colour rather than a text label, and there is no per-cell
/// box. Secondary content (reasoning, tool noise, system) is dimmed; the
/// user line and assistant prose carry the conversation.
///
/// Assistant text always renders as markdown — including while still
/// streaming — so formatting is consistent throughout. `parse_blocks` treats
/// an unclosed code fence as a code block, so a half-streamed fence renders
/// without flicker.
fn cell_lines(cell: &HistoryCell, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let dim = Style::default().fg(Color::DarkGray);
    match cell {
        HistoryCell::Welcome {
            version,
            model,
            offline,
            workspace,
            session,
        } => {
            let cyan = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let label = |key: &str| Span::styled(format!("{key}   "), dim);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("deep-code", cyan),
                    Span::styled(format!("  v{version}"), dim),
                ]),
                Line::from(Span::styled("─".repeat(width.clamp(8, 46)), dim)),
            ];
            if *offline {
                lines.push(Line::from(vec![
                    label("状态"),
                    Span::styled(
                        "离线模式 · 输入 /apikey sk-… 接入 DeepSeek",
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![label("模型"), Span::raw(model.clone())]));
            }
            lines.push(Line::from(vec![
                label("目录"),
                Span::raw(left_truncate(workspace, width.saturating_sub(8).max(8))),
            ]));
            lines.push(Line::from(vec![label("会话"), Span::raw(session.clone())]));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "输入消息开始对话 · 输入 / 查看命令 · Ctrl+C 退出",
                dim,
            )));
            lines.push(Line::default());
            lines
        }
        HistoryCell::User { text } => {
            let mut lines = wrap_prefixed(
                "› ",
                text,
                width,
                Style::default(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            lines.push(Line::default());
            lines
        }
        HistoryCell::Assistant { text } => {
            let mut lines = render_markdown(text, width as u16);
            lines.push(Line::default());
            lines
        }
        HistoryCell::Reasoning { text } => {
            let mut lines = wrap_styled(text, width, dim);
            lines.push(Line::default());
            lines
        }
        // Tool call + result form a tight group: a green dot for the call,
        // a dim ⎿ connector for the result. No blank between them.
        HistoryCell::ToolCall { .. } => {
            let text = cell.lines().join(" ");
            vec![Line::from(vec![
                Span::styled("⏺ ", Style::default().fg(Color::Green)),
                Span::raw(text),
            ])]
        }
        HistoryCell::ToolResult {
            status, summary, ..
        } => {
            let body = match status {
                deep_code_agent::ToolResultStatus::Success => dim,
                deep_code_agent::ToolResultStatus::Denied => Style::default().fg(Color::Yellow),
                deep_code_agent::ToolResultStatus::Error => Style::default().fg(Color::Red),
            };
            let mut lines = wrap_prefixed("  ⎿ ", summary, width, body, dim);
            lines.push(Line::default());
            lines
        }
        HistoryCell::Approval {
            tool_name,
            description,
            risk_level,
            requires_sandbox,
            matched_rule,
            arguments,
        } => {
            let mut lines = approval_lines(
                tool_name,
                risk_level,
                *requires_sandbox,
                matched_rule.as_deref(),
                description,
                arguments,
                width,
            );
            lines.push(Line::default());
            lines
        }
        // Diagnostics / Checkpoint / Compaction / System: dim secondary lines.
        _ => {
            let mut lines = Vec::new();
            for logical in cell.lines() {
                lines.extend(wrap_styled(&logical, width, dim));
            }
            lines.push(Line::default());
            lines
        }
    }
}

/// Keep the rightmost `max` characters of `s`, prefixing `…` when truncated —
/// so a long path shows its meaningful tail (the project directory).
fn left_truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

/// Wrap `text` (honouring embedded newlines) to `width`, styling every row.
fn wrap_styled(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for logical in text.split('\n') {
        for row in wrap_text(logical, width.max(1)) {
            out.push(Line::from(Span::styled(row, style)));
        }
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// Wrap `text` with a marker `prefix` on the first row and a matching indent
/// on continuation rows, so a wrapped block stays visually aligned.
fn wrap_prefixed(
    prefix: &str,
    text: &str,
    width: usize,
    text_style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let pad = UnicodeWidthStr::width(prefix);
    let body_width = width.saturating_sub(pad).max(4);
    let indent = " ".repeat(pad);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        for row in wrap_text(logical, body_width) {
            if out.is_empty() {
                out.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(row, text_style),
                ]));
            } else {
                out.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(row, text_style),
                ]));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(prefix.to_string(), prefix_style)));
    }
    out
}

/// Risk tier (Debug of `RiskLevel`) → (Chinese tag, accent colour). Risk is
/// shown as colour, not a `Risk: …` field. Unknown tiers fall back to amber.
fn risk_display(risk: &str) -> (&'static str, Color) {
    match risk {
        "High" => ("高风险", Color::Red),
        "Medium" => ("中风险", Color::Yellow),
        "Low" => ("低风险", Color::DarkGray),
        _ => ("", Color::Yellow),
    }
}

/// The human-meaningful action behind a tool call — the shell command, the file
/// path, etc. — instead of the raw JSON blob. Falls back to compact arguments.
fn extract_action(arguments_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments_json)
        && let Some(object) = value.as_object()
    {
        for key in ["command", "path", "file_path", "url", "pattern", "query"] {
            if let Some(text) = object.get(key).and_then(serde_json::Value::as_str) {
                return crate::history::collapse_whitespace(text);
            }
        }
    }
    crate::history::collapse_whitespace(arguments_json)
}

/// Minimal, borderless approval block matching the welcome/picker style: a
/// risk-coloured `●` + tool, the action it will take (prominent), an optional
/// dim description, and only meaningful metadata (sandbox / matched rule).
fn approval_lines(
    tool_name: &str,
    risk: &str,
    requires_sandbox: bool,
    matched_rule: Option<&str>,
    description: &str,
    arguments_json: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let (risk_tag, risk_color) = risk_display(risk);
    let risk_style = Style::default().fg(risk_color);

    let mut header = vec![
        Span::styled("● ", risk_style),
        Span::styled("需要批准", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dim),
        Span::styled(
            tool_name.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !risk_tag.is_empty() {
        header.push(Span::styled(" · ", dim));
        header.push(Span::styled(risk_tag, risk_style));
    }
    let mut lines = vec![Line::from(header)];

    let action = crate::history::truncate_chars(&extract_action(arguments_json), 240);
    lines.extend(wrap_prefixed(
        "  ",
        &action,
        width,
        Style::default(),
        Style::default(),
    ));

    let description = description.trim();
    if !description.is_empty() && description != action {
        lines.extend(wrap_prefixed("  ", description, width, dim, dim));
    }

    let mut meta = Vec::new();
    if requires_sandbox {
        meta.push("需沙箱执行".to_string());
    }
    if let Some(rule) = matched_rule {
        meta.push(format!("规则 {rule}"));
    }
    if !meta.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", meta.join(" · ")),
            dim,
        )));
    }
    lines
}

fn render_approval_panel(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let Some(request) = app.pending_approval.as_ref() else {
        return;
    };
    // Body (scrollable) on top; the y/a/n choices pinned to the bottom rows so
    // they stay visible even when a long command wraps.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    let width = usize::from(chunks[0].width.saturating_sub(2)).max(8);
    let body = approval_lines(
        &request.tool_name,
        &format!("{:?}", request.risk_level),
        request.requires_sandbox,
        request.matched_rule.as_deref(),
        &request.description,
        &request.arguments.to_string(),
        width,
    );
    let body_paragraph = Paragraph::new(body)
        .block(Block::default().padding(Padding::new(1, 0, 0, 0)))
        .scroll((app.clamped_approval_scroll_offset() as u16, 0));
    frame.render_widget(body_paragraph, chunks[0]);

    let key_y = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let key_a = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key_n = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let focus = app.approval_focus;
    let options_body: Vec<Line> = [
        ("  y", "批准", key_y),
        ("  a", "本会话始终允许", key_a),
        ("  n", "拒绝（Esc）", key_n),
    ]
    .iter()
    .enumerate()
    .map(|(i, &(key_label, desc, style))| {
        if i == focus {
            let arrow = Span::styled(" ▶", style);
            let key = Span::styled(key_label, style);
            let desc = Span::styled(
                format!("  {desc}"),
                Style::default().add_modifier(Modifier::BOLD),
            );
            Line::from(vec![arrow, key, desc])
        } else {
            let arrow = Span::styled("  ", dim);
            let key = Span::styled(key_label, dim);
            let desc = Span::styled(format!("  {desc}"), dim);
            Line::from(vec![arrow, key, desc])
        }
    })
    .collect();

    let options =
        Paragraph::new(options_body).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(options, chunks[1]);
}

// ---------------------------------------------------------------------------
// layout_input — shared text layout engine for the composer
// ---------------------------------------------------------------------------

/// Pre-computed layout result: the visible subset of wrapped lines, the
/// cursor row/column within that subset, and the total visual row count.
#[derive(Debug, Clone)]
pub(crate) struct LayoutResult {
    /// Lines visible in the viewport (already wrapped).
    pub visible_lines: Vec<String>,
    /// Cursor row relative to the visible subset (0-based).
    pub cursor_visible_row: usize,
    /// Cursor column within its row (0-based, display width).
    pub cursor_col: usize,
    /// Total visual rows across the whole input (for scroll calculations).
    pub total_rows: usize,
}

/// Layout the text, wrapping at `width`, scrolling to keep the cursor
/// visible, and returning only the `max_visible_rows` subset.
pub(crate) fn layout_input(
    input: &str,
    cursor_chars: usize,
    width: usize,
    max_visible_rows: usize,
) -> LayoutResult {
    let lines = wrap_input_lines(input, width);
    let total_rows = lines.len().max(1);
    let max_visible = max_visible_rows.max(1);
    let (cursor_row, cursor_col) = cursor_row_col(input, cursor_chars, width.max(1));

    // Scroll to keep the cursor visible.
    let mut start = 0usize;
    if cursor_row >= max_visible {
        start = cursor_row + 1 - max_visible;
    }
    if start + max_visible > lines.len() {
        start = lines.len().saturating_sub(max_visible);
    }
    let visible = lines[start..start + lines[start..].len().min(max_visible)].to_vec();
    let cursor_visible_row = cursor_row.saturating_sub(start);

    LayoutResult {
        visible_lines: visible,
        cursor_visible_row,
        cursor_col: cursor_col.min(width.saturating_sub(1)),
        total_rows,
    }
}

/// Compute the visual row and column of a character position in a
/// grapheme-cluster-aware way.
fn cursor_row_col(input: &str, cursor_chars: usize, width: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    let mut char_idx = 0usize;

    for grapheme in input.graphemes(true) {
        if char_idx >= cursor_chars {
            break;
        }
        let num_chars = grapheme.chars().count();
        let next_char_idx = char_idx.saturating_add(num_chars);
        let cursor_inside = cursor_chars < next_char_idx;

        if grapheme == "\n" {
            row += 1;
            col = 0;
            char_idx = next_char_idx;
            if cursor_inside {
                break;
            }
            continue;
        }

        let gw = grapheme.width();
        if col + gw > width && col != 0 {
            row += 1;
            col = 0;
        }
        col += gw;
        if col >= width {
            row += 1;
            col = 0;
        }
        if cursor_inside {
            break;
        }
        char_idx = next_char_idx;
    }

    (row, col)
}

/// Split text into logical lines, then wrap each line at `width`.
fn wrap_input_lines(input: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![input.to_string()];
    }
    let mut lines = Vec::new();
    for raw in input.split('\n') {
        let wrapped = wrap_text(raw, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    lines
}

/// Wrap a single (newline-free) text at `width` grapheme-by-grapheme.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for grapheme in text.graphemes(true) {
        let gw = grapheme.width();
        // Start a new line when adding this grapheme would exceed width.
        if current_width + gw > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += gw;
        // If the grapheme alone fills or exceeds the line, finish it.
        if current_width >= width {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_input_from_layout(
    frame: &mut Frame<'_>,
    app: &App,
    layout: &LayoutResult,
    area: ratatui::layout::Rect,
) {
    // Claude-style composer: no box — just a dim rule above and below, with a
    // "› " prompt marker. The streaming state shows in the status line, so the
    // composer needs no title.
    const PROMPT: &str = "› ";
    const GUTTER: u16 = 2;
    let style = Style::default();
    let prompt_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner_area = block.inner(area);
    block.render(area, frame.buffer_mut());

    let inner_height = usize::from(inner_area.height).max(1);
    let text_x = inner_area.x.saturating_add(GUTTER);

    let visible = if layout.visible_lines.len() > inner_height {
        &layout.visible_lines[..inner_height]
    } else {
        &layout.visible_lines
    };
    for (row, line_text) in visible.iter().enumerate() {
        let y = inner_area.y.saturating_add(row as u16);
        if y >= inner_area.y.saturating_add(inner_area.height) {
            break;
        }
        // "› " on the first row, a matching indent on wrapped continuations.
        if row == 0 {
            frame
                .buffer_mut()
                .set_string(inner_area.x, y, PROMPT, prompt_style);
        }
        frame.buffer_mut().set_string(text_x, y, line_text, style);
    }

    // Faint placeholder when empty and idle.
    if app.input.is_empty() && !app.is_streaming && app.pending_approval.is_none() {
        frame.buffer_mut().set_string(
            text_x,
            inner_area.y,
            "输入消息，输入 / 唤起命令",
            Style::default().fg(Color::DarkGray),
        );
    }

    if !app.is_streaming && app.pending_approval.is_none() {
        let cursor_y = inner_area.y.saturating_add(
            u16::try_from(
                layout
                    .cursor_visible_row
                    .min(inner_height.saturating_sub(1)),
            )
            .unwrap_or(u16::MAX),
        );
        let cursor_x = text_x.saturating_add(u16::try_from(layout.cursor_col).unwrap_or(u16::MAX));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let text = if let Some(error) = &app.error {
        Line::from(vec![
            Span::styled(
                "Error: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(error.clone()),
        ])
    } else if let Some(activity) = app.streaming_activity() {
        // While streaming (incl. a long time-to-first-token wait) show an
        // animated indicator so the screen never looks frozen.
        Line::from(vec![
            Span::styled(activity, Style::default().fg(Color::Cyan)),
            Span::styled(
                "   Esc 取消".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(app.status_line())
    };

    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryCell;

    fn line_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    #[test]
    fn streaming_plain_assistant_wraps_to_width() {
        // Assistant text (streaming or flushed) wraps to width, never one row.
        let cell = HistoryCell::Assistant {
            text: "x".repeat(120),
        };
        let lines = cell_lines(&cell, 40);
        assert!(
            lines.len() >= 4,
            "120 cols at width 40 must wrap to multiple rows, got {}",
            lines.len()
        );
        for line in &lines {
            assert!(
                line_width(line) <= 40,
                "row exceeds width: {}",
                line_width(line)
            );
        }
    }

    fn welcome_text(offline: bool) -> String {
        let cell = HistoryCell::Welcome {
            version: "0.1.0".to_string(),
            model: "DeepSeek deepseek-chat · 推理 medium".to_string(),
            offline,
            workspace: "~/code/deep-code".to_string(),
            session: "新会话 · 已持久化".to_string(),
        };
        cell_lines(&cell, 60)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect()
    }

    #[test]
    fn welcome_cell_shows_model_dir_session_when_online() {
        let text = welcome_text(false);
        assert!(text.contains("deep-code") && text.contains("v0.1.0"));
        assert!(text.contains("模型") && text.contains("deepseek-chat"));
        assert!(text.contains("目录") && text.contains("会话"));
        assert!(
            !text.contains("/apikey"),
            "online must not nag about apikey"
        );
    }

    #[test]
    fn welcome_cell_prompts_apikey_when_offline() {
        let text = welcome_text(true);
        assert!(text.contains("离线模式") && text.contains("/apikey"));
        assert!(
            !text.contains("deepseek-chat"),
            "offline hides the model line"
        );
    }

    #[test]
    fn left_truncate_keeps_tail_with_ellipsis() {
        assert_eq!(left_truncate("short", 10), "short");
        assert_eq!(left_truncate("abcdefghij", 5), "…ghij");
    }

    #[test]
    fn extract_action_pulls_command_or_path() {
        assert_eq!(
            extract_action(r#"{"command":"npm run build"}"#),
            "npm run build"
        );
        assert_eq!(
            extract_action(r#"{"path":"src/foo.rs","content":"x"}"#),
            "src/foo.rs"
        );
    }

    #[test]
    fn risk_display_maps_tier_to_colour() {
        assert_eq!(risk_display("High"), ("高风险", Color::Red));
        assert_eq!(risk_display("Medium"), ("中风险", Color::Yellow));
        assert_eq!(risk_display("Low"), ("低风险", Color::DarkGray));
        assert_eq!(risk_display("weird").0, "");
    }

    #[test]
    fn approval_lines_are_minimal_no_dump_fields() {
        let lines = approval_lines(
            "shell_run",
            "Medium",
            false,
            None,
            "运行构建脚本",
            r#"{"command":"npm run build"}"#,
            60,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("需要批准") && text.contains("shell_run"));
        assert!(text.contains("npm run build") && text.contains("中风险"));
        for noise in ["Risk:", "Sandbox:", "Rule:", "Tool:", "Approval required"] {
            assert!(!text.contains(noise), "must not contain `{noise}`");
        }
        // false/none metadata is hidden.
        assert!(!text.contains("沙箱") && !text.contains("规则"));
    }

    #[test]
    fn completion_menu_windows_to_keep_selection_visible() {
        use crate::app::{CompletionKind, CompletionMenu};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let items: Vec<(String, String)> = (0..12)
            .map(|i| (format!("/cmd{i:02}"), String::new()))
            .collect();
        let render_at = |selected: usize| -> String {
            let menu = CompletionMenu {
                kind: CompletionKind::Slash,
                items: items.clone(),
                selected,
            };
            let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
            terminal
                .draw(|frame| render_completion_menu(frame, &menu, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let mut text = String::new();
            for row in 0..buffer.area.height {
                for col in 0..buffer.area.width {
                    text.push_str(buffer[(col, row)].symbol());
                }
            }
            text
        };

        // Wrapping Up from the top lands on the last item — it must stay on screen.
        let bottom = render_at(11);
        assert!(
            bottom.contains("▶ /cmd11"),
            "last item highlighted + visible"
        );
        assert!(
            !bottom.contains("/cmd00"),
            "top items scroll out of the window"
        );

        // Selecting the top item shows it highlighted at the top.
        let top = render_at(0);
        assert!(top.contains("▶ /cmd00"));
    }

    #[test]
    fn streaming_cjk_text_wraps_by_display_width() {
        let cell = HistoryCell::Assistant {
            text: "中".repeat(30), // 60 display columns
        };
        let lines = cell_lines(&cell, 20);
        assert!(lines.len() >= 4);
        for line in &lines {
            assert!(line_width(line) <= 20);
        }
    }
}
