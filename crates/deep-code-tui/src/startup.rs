//! Startup session resolution: a bare `deep-code` opens a fresh session;
//! resuming is opt-in via `-c` (latest) or `-r` (picker). Starting fresh by
//! default keeps stale context from silently leaking into a new task.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use deep_code_agent::{
    AgentConfig, CancellationToken, JsonSessionStore, SessionId, SessionRecord, SessionStore,
    SessionStoreError, format_sessions_storage_note, now_ms,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Padding, Paragraph};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::cli::StartupIntent;
use crate::ui::render::neutralize_display_text;
use deep_code_agent::i18n::{Lang, TextId, tr, tr_with};

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

/// Keep the signal task and its caller responsive during startup file I/O.
/// Cancellation returns to main, which exits without waiting for blocked I/O.
pub(crate) async fn blocking_io<T: Send + 'static>(
    interrupted: &CancellationToken,
    read: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    tokio::select! {
        biased;
        () = interrupted.cancelled() => Err(SessionStoreError::Cancelled.into()),
        result = tokio::task::spawn_blocking(read) => Ok(result?),
    }
}

pub async fn choose_startup(
    store: &JsonSessionStore,
    intent: StartupIntent,
    workspace: &Path,
    interrupted: &CancellationToken,
) -> Result<Option<SessionRecord>> {
    match &intent {
        // A specific id is loaded directly (and surfaces a clear "not found").
        StartupIntent::ResumeId(id) => {
            let id = SessionId::parse(id)?;
            let store = store.clone();
            return Ok(Some(
                blocking_io(interrupted, move || store.load(&id)).await??,
            ));
        }
        // A fresh session needs no listing. `store.list()` fully deserializes
        // every session file on disk; on a long-lived workspace with hundreds
        // of sessions that added a visible stall to the most common launch
        // (bare `deep-code`), all to compute a result we already know is `New`.
        StartupIntent::New => return Ok(None),
        StartupIntent::ContinueLatest | StartupIntent::ResumePicker => {}
    }

    let workspace = workspace.to_path_buf();
    let lang = blocking_io(interrupted, move || {
        Lang::from_env(&AgentConfig::load(&workspace).config.language)
    })
    .await?;
    let diagnostics = crate::interruptible_output::Output::stderr(interrupted)?;
    diagnostics
        .send(format!("{}\n", tr(lang, TextId::StartupScanningSessions)).as_bytes())
        .await?;
    let listing = crate::session_scan::wait_with_interrupt(
        crate::session_scan::SessionScan::start(store.clone())?,
        interrupted.cancelled(),
    )
    .await?;
    let sessions = listing.records; // newest-first, only after a complete scan
    // `list()` collects what it had to skip instead of printing it: here, before
    // the alternate screen, stderr is still the terminal the user is looking at.
    for line in listing.diagnostics {
        diagnostics.send(format!("{line}\n").as_bytes()).await?;
    }
    match resolve(&intent, sessions) {
        Resolution::New => Ok(None),
        Resolution::Resume(record) => Ok(Some(*record)),
        Resolution::Pick(list) => match run_picker(&list, lang, interrupted)? {
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

/// A session worth listing: it has at least one user entry.
pub(crate) fn has_user_message(record: &SessionRecord) -> bool {
    record.has_user_entry()
}

/// First user prompt, single-lined and truncated — the list title.
pub(crate) fn session_title(record: &SessionRecord, lang: Lang) -> String {
    let first = record.entries.iter().find_map(|entry| match &entry.kind {
        deep_code_agent::EntryKind::User { content, .. } => Some(content.as_str()),
        _ => None,
    });
    let first = match first {
        Some(content) => content,
        None => return tr(lang, TextId::EmptySessionTitle).to_string(),
    };
    crate::history::truncate_chars(&crate::history::collapse_whitespace(first), 56)
}

/// Human-readable age, e.g. "刚刚 / 5 分钟前" or "just now / 5 min ago". Pure.
pub(crate) fn relative_time(now_ms: u64, then_ms: u64, lang: Lang) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 60 {
        tr(lang, TextId::TimeJustNow).to_string()
    } else if secs < 3600 {
        tr_with(
            lang,
            TextId::TimeMinutesAgo,
            &[("n", &(secs / 60).to_string())],
        )
    } else if secs < 86_400 {
        tr_with(
            lang,
            TextId::TimeHoursAgo,
            &[("n", &(secs / 3600).to_string())],
        )
    } else {
        tr_with(
            lang,
            TextId::TimeDaysAgo,
            &[("n", &(secs / 86_400).to_string())],
        )
    }
}

fn run_picker(
    sessions: &[SessionRecord],
    lang: Lang,
    interrupted: &CancellationToken,
) -> Result<StartupChoice> {
    let cleanup = crate::ui::TerminalCleanup::new(interrupted, false)?;
    let mut stdout = cleanup.writer();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    // Drop cleanup before Terminal even when drawing/reading returns an error.
    // ratatui's destructor otherwise prints a synchronous stderr error if its
    // attempt to show the cursor uses an already-cancelled writer.
    let mut cleanup = cleanup;
    terminal.clear()?;

    // `record.workspace` is a path deserialized from the session JSON, which
    // lives in the tree the model can write — the same poisoned record that
    // owns the title above owns this line.
    let note = sessions
        .first()
        .map(|r| {
            neutralize_display_text(&format_sessions_storage_note(
                &r.workspace,
                &crate::cli::program_name(),
            ))
        })
        .unwrap_or_default();
    let now = now_ms();
    let mut selected = 0usize;

    let result = loop {
        if interrupted.is_cancelled() {
            cleanup.restore()?;
            return Err(SessionStoreError::Cancelled.into());
        }
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
                    tr(lang, TextId::PickerTitle),
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
                    let time = relative_time(now, record.updated_at_ms, lang);
                    // Same rule, same reason, as the in-app twin in
                    // `ui::render::render_resume_picker`: session records live
                    // at `<workspace>/.deep-code/sessions`, inside the tree the
                    // model can write, `list()` keeps only host-authenticated
                    // records, and `session_title` builds this row
                    // with `split_whitespace`, which keeps `\x1b` intact. This
                    // copy runs on a real backend before `App` exists — it is
                    // the first thing the user sees — and was left unsanitized
                    // when the twin was fixed.
                    let title = neutralize_display_text(&session_title(record, lang));
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
                    tr(lang, TextId::PickerHelpStartup),
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
                    cleanup.restore()?;
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    };

    cleanup.restore()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::SessionEntry;
    use std::path::PathBuf;

    fn session_with(entries: Vec<SessionEntry>, updated_at_ms: u64) -> SessionRecord {
        let mut record = SessionRecord::new(PathBuf::from("/tmp/ws"), "system");
        record.entries = entries.into_iter().map(std::sync::Arc::new).collect();
        record.updated_at_ms = updated_at_ms;
        record
    }

    fn empty() -> SessionRecord {
        session_with(vec![SessionEntry::system("system")], 100)
    }

    fn with_prompt(prompt: &str, ts: u64) -> SessionRecord {
        session_with(
            vec![SessionEntry::system("system"), SessionEntry::user(prompt)],
            ts,
        )
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
            Resolution::Resume(r) => assert_eq!(session_title(&r, Lang::Zh), "recent"),
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
        assert_eq!(relative_time(now, now, Lang::Zh), "刚刚");
        assert_eq!(relative_time(now, now - 90_000, Lang::Zh), "1 分钟前");
        assert_eq!(relative_time(now, now - 7_200_000, Lang::Zh), "2 小时前");
        assert_eq!(relative_time(now, now - 2 * 86_400_000, Lang::Zh), "2 天前");
        assert_eq!(relative_time(now, now - 90_000, Lang::En), "1 min ago");
    }

    #[test]
    fn title_uses_first_user_prompt_single_line() {
        let r = with_prompt("first\nsecond line of prompt", 1);
        assert_eq!(session_title(&r, Lang::Zh), "first second line of prompt");
    }
}
