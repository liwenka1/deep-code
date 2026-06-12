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
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::app::{App, LaunchConfig};
use crate::history::HistoryCell;
use crate::markdown::render_markdown;

/// Redraws are coalesced to at most ~30fps; streaming deltas mark the UI
/// dirty instead of forcing a frame each.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// Periodic full clear to flush any stale cells left by partial redraws.
const FULL_REDRAW_EVERY: u32 = 50;

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
    let mut frames_since_clear = 0u32;

    while !app.should_quit {
        if app.drain_stream_updates() {
            needs_redraw = true;
        }

        let draw_due = last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW_INTERVAL);
        if needs_redraw && draw_due {
            frames_since_clear += 1;
            if frames_since_clear >= FULL_REDRAW_EVERY {
                terminal.clear()?;
                frames_since_clear = 0;
            }
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
        KeyCode::PageUp | KeyCode::Up => app.scroll_up(),
        KeyCode::PageDown | KeyCode::Down => app.scroll_down(),
        KeyCode::End => app.scroll_to_bottom(),
        KeyCode::Char(value) => app.push_char(value),
        _ => {}
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let input_height = Constraint::Length(app.input_height());
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
        render_input(frame, app, chunks[2]);
        render_status(frame, app, chunks[3]);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), input_height, Constraint::Length(1)])
            .split(frame.area());
        render_messages(frame, app, chunks[0]);
        render_input(frame, app, chunks[1]);
        render_status(frame, app, chunks[2]);
    }
}

fn render_messages(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let mut items = Vec::new();
    let history_len = app.history.len();
    // Borrow history and chain the (small) active preview instead of deep
    // cloning the whole transcript every frame.
    let preview = app
        .active_turn
        .as_ref()
        .map(|active| active.preview_cells())
        .unwrap_or_default();
    let total = history_len + preview.len();
    let content_width = area.width.saturating_sub(2).max(8);
    let visible_messages = usize::from(area.height.saturating_sub(2)).saturating_div(3);
    let visible_messages = visible_messages.max(1);
    let bottom_skip = total.saturating_sub(visible_messages);
    let skip_count = bottom_skip.saturating_sub(app.scroll_offset);

    for (index, cell) in app
        .history
        .iter()
        .chain(preview.iter())
        .enumerate()
        .skip(skip_count)
        .take(visible_messages)
    {
        let mut lines = vec![Line::from(label_for_cell(cell))];
        // Flushed assistant cells render as markdown; the still-streaming
        // preview stays plain so half-open code fences don't flicker.
        match cell {
            HistoryCell::Assistant { text } if index < history_len => {
                lines.extend(render_markdown(text, content_width));
            }
            _ => lines.extend(cell.lines().into_iter().map(Line::from)),
        }
        lines.push(Line::default());
        items.push(ListItem::new(lines));
    }

    let list = List::new(items).block(
        Block::default()
            .title("deep-code")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(list, area);
}

fn render_approval_panel(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let Some(cell) = app.approval_cell() else {
        return;
    };
    let visible_lines = usize::from(area.height.saturating_sub(2)).max(1);
    let mut lines = vec![Line::from(
        "Keys: y approve | n/Esc deny | PageUp/PageDown scroll",
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

fn render_input(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let title = if app.is_streaming {
        "Prompt (streaming...)"
    } else {
        "Prompt"
    };
    let paragraph = Paragraph::new(app.input.as_str())
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
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
