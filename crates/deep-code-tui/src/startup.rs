//! Startup session resolution. Mirrors Claude Code: a bare `deep-code` opens
//! a fresh session; resuming is opt-in via `-c` (latest) or `-r` (picker).

use std::io::{self, Stdout};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use deep_code_agent::{
    JsonSessionStore, Role, SessionId, SessionRecord, SessionStore, format_sessions_storage_note,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Padding, Paragraph};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::cli::StartupIntent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupChoice {
    NewSession,
    Resume(usize),
}

/// What [`choose_startup`] decides before any UI is shown — kept pure so it
/// is unit-testable; the interactive picker is a separate step.
#[derive(Debug, Clone, PartialEq)]
enum Resolution {
    New,
    Resume(Box<SessionRecord>),
    Pick(Vec<SessionRecord>),
}

pub fn choose_startup(
    store: &JsonSessionStore,
    intent: StartupIntent,
) -> Result<Option<SessionRecord>> {
    // A specific id is loaded directly (and surfaces a clear "not found").
    if let StartupIntent::ResumeId(id) = &intent {
        return Ok(Some(store.load(&SessionId::parse(id)?)?));
    }

    let sessions = store.list()?; // newest-first
    match resolve(&intent, sessions) {
        Resolution::New => Ok(None),
        Resolution::Resume(record) => Ok(Some(*record)),
        Resolution::Pick(list) => match run_picker(&list)? {
            StartupChoice::NewSession => Ok(None),
            StartupChoice::Resume(index) => Ok(list.into_iter().nth(index)),
        },
    }
}

/// Decide the startup target from the intent and the (newest-first) session
/// list, ignoring empty sessions (no user message). Pure for testability.
fn resolve(intent: &StartupIntent, sessions: Vec<SessionRecord>) -> Resolution {
    let real: Vec<SessionRecord> = sessions.into_iter().filter(has_user_message).collect();
    match intent {
        StartupIntent::New | StartupIntent::ResumeId(_) => Resolution::New,
        StartupIntent::ContinueLatest => real
            .into_iter()
            .next()
            .map_or(Resolution::New, |r| Resolution::Resume(Box::new(r))),
        StartupIntent::ResumePicker => {
            if real.is_empty() {
                Resolution::New
            } else {
                Resolution::Pick(real)
            }
        }
    }
}

/// A session worth listing: it has at least one user message.
pub(crate) fn has_user_message(record: &SessionRecord) -> bool {
    record.messages.iter().any(|m| m.role == Role::User)
}

/// First user prompt, single-lined and truncated — the list title.
pub(crate) fn session_title(record: &SessionRecord) -> String {
    let first = record
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .unwrap_or("(空会话)");
    truncate(&first.split_whitespace().collect::<Vec<_>>().join(" "), 56)
}

/// Human-readable age, e.g. "刚刚 / 5 分钟前 / 3 小时前 / 2 天前". Pure.
pub(crate) fn relative_time(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86_400)
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn run_picker(sessions: &[SessionRecord]) -> Result<StartupChoice> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let note = sessions
        .first()
        .map(|r| format_sessions_storage_note(&r.workspace))
        .unwrap_or_default();
    let now = now_ms();
    let mut selected = 0usize;

    let result = loop {
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(3),
                    Constraint::Length(2),
                ])
                .split(frame.area());

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

            // Window the list so the selection stays visible.
            let viewport = usize::from(chunks[1].height).max(1);
            let start = selected.saturating_sub(viewport.saturating_sub(1));
            let rows: Vec<Line> = sessions
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
            let list =
                Paragraph::new(rows).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
            frame.render_widget(list, chunks[1]);

            let help = Paragraph::new(vec![
                Line::from(Span::styled(
                    "↑/↓ 选择 · Enter 恢复 · n/Esc 新会话 · Ctrl+C 退出",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    note.clone(),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(Block::default().padding(Padding::new(1, 0, 0, 0)));
            frame.render_widget(help, chunks[2]);
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
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::{AgentConfig, Message};
    use std::path::PathBuf;

    fn session_with(messages: Vec<Message>, updated_at_ms: u64) -> SessionRecord {
        let mut record =
            SessionRecord::new(PathBuf::from("/tmp/ws"), &AgentConfig::builtin(), "system");
        record.messages = messages;
        record.updated_at_ms = updated_at_ms;
        record
    }

    fn empty() -> SessionRecord {
        session_with(vec![Message::system("system")], 100)
    }

    fn with_prompt(prompt: &str, ts: u64) -> SessionRecord {
        session_with(vec![Message::system("system"), Message::user(prompt)], ts)
    }

    #[test]
    fn new_intent_starts_fresh() {
        assert_eq!(
            resolve(&StartupIntent::New, vec![with_prompt("hi", 1)]),
            Resolution::New
        );
    }

    #[test]
    fn continue_resumes_latest_non_empty() {
        // Newest-first input; the empty one is skipped.
        let sessions = vec![
            empty(),
            with_prompt("recent", 200),
            with_prompt("older", 100),
        ];
        match resolve(&StartupIntent::ContinueLatest, sessions) {
            Resolution::Resume(r) => assert_eq!(session_title(&r), "recent"),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn continue_with_no_real_sessions_is_new() {
        assert_eq!(
            resolve(&StartupIntent::ContinueLatest, vec![empty()]),
            Resolution::New
        );
    }

    #[test]
    fn picker_filters_empty_sessions() {
        let sessions = vec![with_prompt("a", 2), empty(), with_prompt("b", 1)];
        match resolve(&StartupIntent::ResumePicker, sessions) {
            Resolution::Pick(list) => {
                assert_eq!(list.len(), 2, "empty session excluded");
                assert!(list.iter().all(has_user_message));
            }
            other => panic!("expected Pick, got {other:?}"),
        }
    }

    #[test]
    fn picker_with_only_empty_is_new() {
        assert_eq!(
            resolve(&StartupIntent::ResumePicker, vec![empty(), empty()]),
            Resolution::New
        );
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "刚刚");
        assert_eq!(relative_time(now, now - 90_000), "1 分钟前");
        assert_eq!(relative_time(now, now - 7_200_000), "2 小时前");
        assert_eq!(relative_time(now, now - 2 * 86_400_000), "2 天前");
    }

    #[test]
    fn title_uses_first_user_prompt_single_line() {
        let r = with_prompt("first\nsecond line of prompt", 1);
        assert_eq!(session_title(&r), "first second line of prompt");
    }
}
