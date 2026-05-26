//! Startup session picker shown before the main TUI loop.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use deep_code_agent::{
    JsonSessionStore, SessionRecord, SessionStore, format_sessions_storage_note,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Line, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Terminal, backend::CrosstermBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupChoice {
    NewSession,
    Resume(usize),
}

pub fn choose_startup(
    store: &JsonSessionStore,
    force_new: bool,
    resume: Option<&str>,
) -> Result<Option<SessionRecord>> {
    let sessions = store.list()?;

    if force_new {
        return Ok(None);
    }

    if let Some(token) = resume {
        if token == "latest" {
            return sessions
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no saved sessions to resume"))
                .map(Some);
        }
        return Ok(Some(
            store.load(&deep_code_agent::SessionId::parse(token)?)?,
        ));
    }

    if sessions.is_empty() {
        return Ok(None);
    }

    match run_picker(&sessions)? {
        StartupChoice::NewSession => Ok(None),
        StartupChoice::Resume(index) => Ok(sessions.into_iter().nth(index)),
    }
}

fn storage_note(sessions: &[SessionRecord]) -> String {
    sessions
        .first()
        .map(|record| format_sessions_storage_note(&record.workspace))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|cwd| format_sessions_storage_note(&cwd))
                .unwrap_or_else(|_| "Sessions are stored per workspace directory.".to_string())
        })
}

fn run_picker(sessions: &[SessionRecord]) -> Result<StartupChoice> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let note = storage_note(sessions);
    let mut selected = 0usize;
    let result = loop {
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(3)])
                .split(frame.area());

            let items = sessions
                .iter()
                .enumerate()
                .map(|(index, record)| {
                    let marker = if index == selected { ">" } else { " " };
                    let preview = record.preview().replace('\n', " ");
                    ListItem::new(Line::from(format!(
                        "{marker} {}  ({})  {}",
                        record.id.as_str(),
                        record.messages.len(),
                        truncate(&preview, 48)
                    )))
                })
                .collect::<Vec<_>>();

            let list = List::new(items).block(
                Block::default()
                    .title("Resume a session?")
                    .borders(Borders::ALL),
            );
            frame.render_widget(list, chunks[0]);

            let help = Paragraph::new(format!(
                "{note}\n↑/↓ select  Enter resume  n new session  Esc new session  Ctrl+C quit"
            ))
            .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(help, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < sessions.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => break StartupChoice::Resume(selected),
                KeyCode::Char('n') | KeyCode::Esc => break StartupChoice::NewSession,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    restore_terminal(&mut terminal)?;
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    };

    restore_terminal(&mut terminal)?;
    Ok(result)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.to_string();
        };
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}
