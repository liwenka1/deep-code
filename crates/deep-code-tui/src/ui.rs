use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style, Stylize};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::{Terminal, backend::CrosstermBackend};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, LaunchConfig};
use crate::history::HistoryCell;
use crate::markdown::render_markdown;

/// Redraws are coalesced to at most ~30fps; streaming deltas mark the UI
/// dirty instead of forcing a frame each.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// While streaming, repaint at least this often so the activity indicator
/// animates through a long time-to-first-token wait.
const STREAM_TICK_INTERVAL: Duration = Duration::from_millis(120);
type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(config: LaunchConfig) -> Result<()> {
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
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    Ok(terminal)
}

fn restore_terminal(terminal: &mut AppTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn run_loop(terminal: &mut AppTerminal, app: &mut App) -> Result<()> {
    let mut needs_redraw = true;
    let mut last_draw: Option<Instant> = None;

    while !app.should_quit {
        if app.drain_stream_updates() {
            needs_redraw = true;
        }

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
                Event::Resize(..) => needs_redraw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.approve_pending_tool(),
            KeyCode::Char('a') | KeyCode::Char('A') => app.approve_pending_tool_for_session(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.deny_pending_tool(),
            KeyCode::PageUp | KeyCode::Up => app.scroll_approval_up(),
            KeyCode::PageDown | KeyCode::Down => app.scroll_approval_down(),
            KeyCode::Home => app.scroll_approval_to_top(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
            }
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
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => app.push_newline(),
        KeyCode::Enter => app.submit(),
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => app.push_newline(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => app.history_prev(),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => app.history_next(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward(),
        KeyCode::Left => app.cursor_left(),
        KeyCode::Right => app.cursor_right(),
        KeyCode::Home => app.cursor_home(),
        KeyCode::End => app.cursor_end(),
        KeyCode::PageUp | KeyCode::Up => app.scroll_up(),
        KeyCode::PageDown | KeyCode::Down => app.scroll_down(),
        KeyCode::Char(value) => app.push_char(value),
        _ => {}
    }
}

/// Soft upper bound for composer visible rows before scrolling.
pub(crate) const COMPOSER_MAX_VISIBLE_ROWS: usize = 6;

fn render(frame: &mut Frame<'_>, app: &App) {
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

    if app.pending_approval.is_some() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(8),
                input_height,
                Constraint::Length(1),
            ])
            .split(frame.area());
        render_messages(frame, app, chunks[0]);
        render_approval_panel(frame, app, chunks[1]);
        render_input_from_layout(frame, app, &layout, chunks[2]);
        render_status(frame, app, chunks[3]);
    } else if let Some(menu) = &app.completion {
        let menu_height = (menu.items.len() as u16).min(8) + 2;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(menu_height),
                input_height,
                Constraint::Length(1),
            ])
            .split(frame.area());
        render_messages(frame, app, chunks[0]);
        render_completion_menu(frame, menu, chunks[1]);
        render_input_from_layout(frame, app, &layout, chunks[2]);
        render_status(frame, app, chunks[3]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), input_height, Constraint::Length(1)])
            .split(frame.area());
        render_messages(frame, app, chunks[0]);
        render_input_from_layout(frame, app, &layout, chunks[1]);
        render_status(frame, app, chunks[2]);
    }
}

fn render_completion_menu(
    frame: &mut Frame<'_>,
    menu: &crate::app::CompletionMenu,
    area: ratatui::layout::Rect,
) {
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .take(8)
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

fn render_messages(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let viewport = usize::from(area.height.saturating_sub(2)).max(1);
    let content_width = area.width.saturating_sub(2).max(8);
    let history_len = app.history.len();
    // Borrow history and chain the (small) active preview instead of deep
    // cloning the whole transcript every frame.
    let preview = app
        .active_turn
        .as_ref()
        .map(|active| active.preview_cells())
        .unwrap_or_default();

    // Visual-line scroll model: walk cells from the bottom, rendering only
    // until the viewport plus the scroll-back distance is covered. This
    // replaces the old "3 lines per cell" estimate that markdown broke.
    let target = viewport + app.scroll_offset;
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut total_lines = 0usize;
    let total_cells = history_len + preview.len();
    let mut index = total_cells;
    while index > 0 && total_lines < target {
        index -= 1;
        let cell = if index < history_len {
            &app.history[index]
        } else {
            &preview[index - history_len]
        };
        let lines = cell_lines(cell, content_width);
        total_lines += lines.len();
        chunks.push(lines);
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total_lines);
    for chunk in chunks.into_iter().rev() {
        lines.extend(chunk);
    }

    // Bottom-anchored: scroll_offset visual lines up from the end, clamped
    // to the rendered range.
    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = app.scroll_offset.min(max_scroll);
    let scroll_top = max_scroll - scroll;

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("deep-code")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .scroll((scroll_top as u16, 0));
    frame.render_widget(paragraph, area);
}

/// Render one transcript cell: label line, body, trailing blank.
///
/// Assistant text always renders as markdown — including while still
/// streaming — so the formatting is consistent throughout instead of
/// showing raw `#`/`*`/backticks mid-stream and snapping to formatted on
/// completion. `parse_blocks` treats an unclosed code fence as a code block,
/// so a half-streamed fence renders cleanly without flicker.
fn cell_lines(cell: &HistoryCell, width: u16) -> Vec<Line<'static>> {
    // Tool call/result are rendered as one tight colored line (no label box,
    // no trailing blank) so a multi-step tool sequence stacks compactly.
    match cell {
        HistoryCell::ToolCall { .. } => {
            let text = cell.lines().join(" ");
            return vec![Line::from(Span::raw(format!("⏺ {text}")).yellow())];
        }
        HistoryCell::ToolResult { status, .. } => {
            let text = cell.lines().join(" ");
            let styled = Span::raw(format!("  {text}"));
            let styled = match status {
                deep_code_agent::ToolResultStatus::Success => styled.green(),
                deep_code_agent::ToolResultStatus::Denied => styled.yellow(),
                deep_code_agent::ToolResultStatus::Error => styled.red(),
            };
            return vec![Line::from(styled)];
        }
        _ => {}
    }
    let mut lines = vec![Line::from(label_for_cell(cell))];
    match cell {
        HistoryCell::Assistant { text } => {
            lines.extend(render_markdown(text, width));
        }
        // Other plain cells (reasoning, system, user, etc.) must be wrapped
        // to the viewport width here: the messages Paragraph is
        // scroll-positioned, not `.wrap()`-ed, so an unwrapped long line
        // would fill one row and get clipped.
        _ => {
            for logical in cell.lines() {
                for wrapped in wrap_text(&logical, width as usize) {
                    lines.push(Line::from(wrapped));
                }
            }
        }
    }
    lines.push(Line::default());
    lines
}

fn render_approval_panel(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let Some(cell) = app.approval_cell() else {
        return;
    };
    let visible_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let mut lines = vec![Line::from(
        "Keys: y approve | a 本会话总是允许 | n/Esc deny | PageUp/PageDown scroll",
    )];
    lines.extend(
        cell.lines()
            .into_iter()
            .skip(app.clamped_approval_scroll_offset())
            .take(visible_lines.saturating_sub(1))
            .map(Line::from),
    );
    let panel = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Approval required")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
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
    let title = if app.is_streaming {
        "Prompt (streaming...)"
    } else {
        "Prompt"
    };
    let style = Style::default();
    let inner_width = usize::from(area.width.saturating_sub(2)).max(1);
    let inner_height = usize::from(area.height.saturating_sub(2)).max(1);

    // Render block borders first.
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner_area = block.inner(area);
    block.render(area, frame.buffer_mut());

    // Write each visible line directly — no Paragraph wrapping.
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
        frame.buffer_mut().set_string(inner_area.x, y, line_text, style);
    }

    if !app.is_streaming && app.pending_approval.is_none() {
        let cursor_y = inner_area.y.saturating_add(
            u16::try_from(layout.cursor_visible_row.min(inner_height.saturating_sub(1)))
                .unwrap_or(u16::MAX),
        );
        let cursor_x = inner_area.x.saturating_add(
            u16::try_from(layout.cursor_col.min(inner_width.saturating_sub(1)))
                .unwrap_or(u16::MAX),
        );
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

fn label_for_cell(cell: &HistoryCell) -> Span<'static> {
    match cell {
        HistoryCell::User { .. } => cell.label().blue().bold(),
        HistoryCell::Assistant { .. } => cell.label().green().bold(),
        HistoryCell::Reasoning { .. } => cell.label().cyan().bold(),
        HistoryCell::ToolCall { .. } => cell.label().yellow().bold(),
        HistoryCell::ToolResult { .. } => cell.label().magenta().bold(),
        HistoryCell::Approval { .. } => cell.label().yellow().bold(),
        HistoryCell::Diagnostics { .. } => cell.label().red().bold(),
        HistoryCell::Checkpoint { .. } => cell.label().dark_gray().bold(),
        HistoryCell::Compaction { .. } => cell.label().dark_gray().bold(),
        HistoryCell::System { .. } => cell.label().dark_gray().bold(),
    }
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
            assert!(line_width(line) <= 40, "row exceeds width: {}", line_width(line));
        }
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
