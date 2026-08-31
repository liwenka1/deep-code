//! All frame rendering: transcript cells, resume picker, completion menu,
//! approval panel, composer input, and status bar, plus the text-wrapping
//! helpers they share.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style, Stylize};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Widget};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, TranscriptSnapshot};
use crate::history::HistoryCell;
use crate::markdown::render_markdown;
use deep_code_agent::SafetyNote;
use deep_code_agent::i18n::{Lang, TextId, tr, tr_with};
// The shared core of every sanitizer in the workspace. It lives in the agent
// crate because `deep-code-runtime` and the non-TUI CLI surfaces have to reach
// it too — see `deep_code_agent::text_sanitize`.
use deep_code_agent::{is_bidi_or_zero_width, neutralize_char_into};

use super::COMPOSER_MAX_VISIBLE_ROWS;

mod approval;
mod completion;
mod composer;
mod transcript;

// Flatten the submodules back into this namespace: the split is file
// organisation, not an API boundary — helpers cross-reference each other
// (and `tests.rs` reaches everything) exactly as before via `super::*`.
use approval::*;
use completion::*;
use composer::*;
use transcript::*;
// Items with callers outside `ui::render` keep their crate-visible paths.
// (`LayoutResult` is deliberately absent: `layout_input`'s callers outside
// this module never name the type, so re-exporting it would be dead.)
pub(crate) use composer::{layout_input, neutralize_composer_text};
pub(crate) use transcript::sanitize_for_clipboard;

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) {
    if let Some(picker) = &app.resume_picker {
        render_resume_picker(frame, picker, app.lang);
        return;
    }

    let inner_width = frame.area().width.saturating_sub(2).max(1);
    // Compute layout once — height and rendering share the same result.
    let composer_text = neutralize_composer_text(&app.input);
    let layout = layout_input(
        &composer_text,
        app.input_cursor,
        inner_width as usize,
        COMPOSER_MAX_VISIBLE_ROWS,
    );
    let visual_rows = layout.total_rows.clamp(1, COMPOSER_MAX_VISIBLE_ROWS);
    let input_height = Constraint::Length(visual_rows as u16 + 2);

    let snapshot: TranscriptSnapshot = if app.pending_approval.is_some() {
        // Carved by hand, not by the constraint solver. `Constraint::Min(5)`
        // on the transcript carries MIN_SIZE_GE strength (STRONG*100) while
        // the panel's `Constraint::Length` carries LENGTH_SIZE_EQ (STRONG*10),
        // so on any frame too short for both, the transcript took its five
        // rows and the panel absorbed the entire deficit: at 80x12 a panel
        // asking for 6 rows was handed 3, its body 1, and the overflow hint 0
        // — the human saw a single header line naming no directory, with
        // nothing to say more existed, and `y` still granted.
        //
        // Priority when the frame cannot hold everything: status row, then
        // the panel in full, then the composer, and the transcript takes what
        // is left (possibly nothing). Answering an approval needs neither a
        // composer nor scrollback.
        let area = frame.area();
        let panel_rows = approval_panel_rows(app, area);
        let mut rest = area.height;
        let status_h = rest.min(1);
        rest -= status_h;
        let panel_h = panel_rows.min(rest);
        rest -= panel_h;
        let input_h = (visual_rows as u16 + 2).min(rest);
        rest -= input_h;
        let transcript_h = rest;

        let row = |offset: u16, height: u16| ratatui::layout::Rect {
            x: area.x,
            y: area.y + offset,
            width: area.width,
            height,
        };
        let snap = render_messages(frame, app, row(0, transcript_h));
        render_approval_panel(frame, app, row(transcript_h, panel_h));
        render_input_from_layout(frame, app, &layout, row(transcript_h + panel_h, input_h));
        render_status(frame, app, row(transcript_h + panel_h + input_h, status_h));
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
        render_completion_menu(frame, menu, chunks[1], app.lang);
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
fn render_resume_picker(frame: &mut Frame<'_>, picker: &crate::app::ResumePicker, lang: Lang) {
    use crate::startup::{relative_time, session_title};
    use deep_code_agent::now_ms;
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
            tr(lang, TextId::PickerTitle),
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
            let time = relative_time(now, record.updated_at_ms, lang);
            // Session records live at `<workspace>/.deep-code/sessions`, inside
            // the tree the model can write, and `session_title` builds the row
            // from the first user message with `split_whitespace` — which keeps
            // `\x1b` (not whitespace) intact. Sanitize like every other
            // model-reachable string.
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
    let list = Paragraph::new(rows).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(list, chunks[1]);

    // `record.workspace` is a path deserialized from the session JSON, which
    // lives in the tree the model can write — the same poisoned record that
    // owns the title above owns this line, 30 rows further down the function.
    let note = picker
        .sessions
        .first()
        .map(|record| {
            neutralize_display_text(&deep_code_agent::format_sessions_storage_note(
                &record.workspace,
            ))
        })
        .unwrap_or_default();
    let help = Paragraph::new(vec![
        Line::from(Span::styled(
            tr(lang, TextId::PickerHelpResume),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(note, Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(help, chunks[2]);
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

/// [`neutralize_char_into`] over a whole string: control characters become
/// spaces, bidi/zero-width are deleted. ratatui carries an escape byte through
/// to the terminal verbatim, so text reaching a panel line with `\x1b` or `\r`
/// intact can erase or overwrite the lines around it — inside the very prompt
/// the human is reading to decide.
///
/// The bidi/zero-width half is OURS, not ratatui's. This function used to say
/// zero-width needed no handling because ratatui drops those before they reach
/// a cell — measured, true, and undocumented. The transcript stopped renting
/// that in 9cb8e76 while the approval panel kept renting it, which left the
/// one surface whose whole job is being read correctly (a bidi override can
/// reorder a resolved grant target in the same frame) depending on a
/// dependency's incidental behavior. Both paths now share
/// [`neutralize_char_into`].
///
/// Kept separate from [`sanitize_panel_text`] for the two callers that must not
/// trim or cap: a diff line's leading spaces carry its alignment, and a tool
/// description is longer than any line cap.
pub(crate) use deep_code_agent::neutralize_display_text;

fn render_status(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    use deep_code_agent::PermissionMode;

    // A permanent chip so the current permission mode is always visible.
    // Yolo shouts (red bold); the rest are quiet.
    let mode = app.permission_mode();
    let chip_style = match mode {
        PermissionMode::Yolo => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        PermissionMode::Auto => Style::default().fg(Color::Yellow),
        PermissionMode::AcceptEdits => Style::default().fg(Color::Cyan),
        PermissionMode::Default => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![Span::styled(
        format!("[{}] ", crate::app::perm_mode_label(app.lang, mode)),
        chip_style,
    )];

    if let Some(error) = &app.error {
        spans.push(Span::styled(
            tr(app.lang, TextId::ErrorPrefix),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        // `RuntimeEvent::Error` carries tool failures, which quote the paths
        // and commands the model chose — and this row is drawn in the SAME
        // frame as the approval panel, outliving the turn that produced it.
        // An escape here reaches the terminal exactly like one in the
        // transcript did, so it gets the same treatment.
        //
        // ALL THREE branches, not just this one. The other two carry the tool
        // NAME straight off the model's tool call — `streaming_activity`
        // formats `ActiveToolCell::tool_name`, and `status_line` splices
        // `App::status`, which `event_routing` fills from `tool_name` on every
        // `ToolCallStarted` and on `ApprovalRequired`. That last one prints
        // the model's chosen name on this row in the same frame as the
        // approval panel — and this row is drawn AFTER the panel, so an
        // escape here repaints the thing the human is reading to decide.
        // Sanitizing only the error branch is why that survived three rounds:
        // the one test scanning this row populates `app.error`, so the `if`
        // shadowed both `else`s.
        spans.push(Span::raw(neutralize_display_text(error)));
    } else if let Some(activity) = app.streaming_activity() {
        // While streaming (incl. a long time-to-first-token wait) show an
        // animated indicator so the screen never looks frozen.
        spans.push(Span::styled(
            neutralize_display_text(&activity),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(
            format!("   {}", tr(app.lang, TextId::StatusEscCancel)),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::raw(neutralize_display_text(&app.status_line())));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests;
