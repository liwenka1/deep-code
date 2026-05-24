use std::io::{self, Stdout};
use std::time::Duration;

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

use crate::app::{App, Author, LaunchConfig};

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
    while !app.should_quit {
        app.drain_stream_updates();
        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
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
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter => app.submit(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Char(value) => app.push_char(value),
        _ => {}
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_messages(frame, app, chunks[0]);
    render_input(frame, app, chunks[1]);
    render_status(frame, app, chunks[2]);
}

fn render_messages(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let mut items = Vec::new();
    let visible_messages = usize::from(area.height.saturating_sub(2)).saturating_div(3);
    let skip_count = app.messages.len().saturating_sub(visible_messages.max(1));

    for message in app.messages.iter().skip(skip_count) {
        items.push(ListItem::new(vec![
            Line::from(label_for_author(&message.author)),
            Line::from(message.text.clone()),
            Line::default(),
        ]));
    }

    if !app.streaming_buffer.is_empty() {
        items.push(ListItem::new(vec![
            Line::from(label_for_author(&Author::Assistant)),
            Line::from(app.streaming_buffer.clone()),
        ]));
    }

    if let Some(request) = &app.pending_approval {
        let sandbox = if request.requires_sandbox {
            "yes (OS sandbox when available)"
        } else {
            "no"
        };
        let rule = request
            .matched_rule
            .as_deref()
            .unwrap_or("none");
        items.push(ListItem::new(vec![
            Line::from("Approval required".yellow().bold()),
            Line::from(format!("Tool: {}", request.tool_name)),
            Line::from(format!("Risk: {:?} | Sandbox: {sandbox}", request.risk_level)),
            Line::from(format!("Rule: {rule}")),
            Line::from(format!("Description: {}", request.description)),
            Line::from(format!("Arguments: {}", request.arguments)),
            Line::from("Press y to approve, n to deny."),
        ]));
    }

    let list = List::new(items).block(
        Block::default()
            .title("deep-code")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(list, area);
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
        Line::from(app.status.clone())
    };

    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn label_for_author(author: &Author) -> Span<'static> {
    match author {
        Author::User => "You".blue().bold(),
        Author::Assistant => "Assistant".green().bold(),
        Author::System => "System".dark_gray().bold(),
    }
}
