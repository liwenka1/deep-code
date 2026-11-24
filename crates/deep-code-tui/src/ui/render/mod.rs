//! All frame rendering: transcript cells, resume picker, completion menu,
//! approval panel, composer input, and status bar, plus the text-wrapping
//! helpers they share.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style, Stylize};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Widget};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ComposerSnapshot, FoldTarget, QuitKey, TranscriptSnapshot};
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
pub(crate) use composer::{char_index_at, layout_input, neutralize_composer_text};
pub(crate) use transcript::sanitize_for_clipboard;

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) {
    if let Some(picker) = &app.resume_picker {
        app.yolo_confirmation.set_visible(None);
        render_resume_picker(frame, picker, app.lang);
        // The overlay covers the composer: nothing of it is there to click.
        app.composer = None;
        return;
    }
    let inner_width = frame.area().width.saturating_sub(2).max(1);
    // Compute layout once — height and rendering share the same result.
    let composer_text = neutralize_composer_text(&app.input);
    let searching = app.history_search_focused();
    let layout = if searching {
        layout_recalled_input(
            &composer_text,
            inner_width as usize,
            COMPOSER_MAX_VISIBLE_ROWS,
            app.composer_scroll,
        )
    } else {
        layout_input(
            &composer_text,
            app.input_cursor,
            inner_width as usize,
            COMPOSER_MAX_VISIBLE_ROWS,
            app.composer_scroll,
        )
    };
    let visual_rows = layout.total_rows.clamp(1, COMPOSER_MAX_VISIBLE_ROWS) as u16;
    let area = frame.area();

    // The composer's text rows, when it takes clicks. Under an approval panel it
    // is drawn but not editable — its keys are y/a/n — so it takes none.
    let (snapshot, composer_rows) = if searching {
        app.yolo_confirmation.set_visible(None);
        let badges = status_badges(app);
        let badges_width: usize = badges.iter().map(Span::width).sum();
        let width = usize::from(area.width);
        let separate_row =
            app.prepare_history_search_field(width.saturating_sub(badges_width), width);
        // Reserve the field before assigning the recalled prompt and
        // transcript. With just one visible line, the active field takes it.
        let [body, search, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(area.height.min(1)),
            Constraint::Length(u16::from(separate_row && area.height > 1)),
        ])
        .areas(area);
        // The caret is in the search row. One recalled row is enough before
        // reserving transcript space for output and scrolling.
        let [transcript_height, _, composer_height] =
            body_row_heights(body.height, visual_rows, 1, 0);
        let [transcript, composer] = Layout::vertical([
            Constraint::Length(transcript_height),
            Constraint::Length(composer_height),
        ])
        .areas(body);
        let snap = render_messages(frame, app, transcript);
        let rows = render_input_from_layout(frame, app, &layout, composer);
        let query_badges = if separate_row {
            frame.render_widget(Line::from(badges), status);
            Vec::new()
        } else {
            badges
        };
        render_history_search_field(frame, search, query_badges, app);
        (snap, Some(rows))
    } else if app.pending_approval.is_some() {
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
        // Built once for the whole frame: the layout below sizes the panel from
        // these lines and `render_approval_panel` draws exactly them, so the
        // reserved height cannot drift from the drawn content — and the request's
        // argument JSON is serialized once per frame rather than three times.
        // The width matches what the panel itself derives: this branch gives the
        // panel a row of the full frame width, and a vertical split preserves it.
        let panel = build_approval_panel(app, usize::from(area.width.saturating_sub(2)).max(8))
            .expect("this branch runs only while an approval is pending");
        let panel_rows = approval_panel_rows(&panel, area);
        let mut rest = area.height;
        let status_h = rest.min(1);
        rest -= status_h;
        let panel_h = panel_rows.min(rest);
        rest -= panel_h;
        let input_h = (visual_rows + 2).min(rest);
        rest -= input_h;
        let transcript_h = rest;

        let row = |offset: u16, height: u16| ratatui::layout::Rect {
            x: area.x,
            y: area.y + offset,
            width: area.width,
            height,
        };
        let snap = render_messages(frame, app, row(0, transcript_h));
        render_approval_panel(frame, app, panel, row(transcript_h, panel_h));
        render_input_from_layout(frame, app, &layout, row(transcript_h + panel_h, input_h));
        render_status(frame, app, row(transcript_h + panel_h + input_h, status_h));
        (snap, None)
    } else {
        // Cloned for the frame: `render_messages` now takes `&mut App` (it
        // memoizes each cell's rendered lines), so nothing may hold a borrow
        // into `app` across it. The menu is a handful of short rows.
        let menu = app.completion.clone();
        let menu_height = menu.as_ref().map_or(0, |menu| {
            menu.items.len().min(COMPLETION_VISIBLE_ROWS) as u16 + 2
        });
        let status_height = if app.yolo_confirmation_active() {
            let badges = Line::from(status_badges(app));
            let (_, rows) = yolo_confirmation_layout(&badges, area, app.lang);
            rows.min(area.height)
        } else {
            // The draft takes the only row in a one-line pane.
            u16::from(area.height > 1)
        };
        let [transcript_height, menu_height, composer_height] = body_row_heights(
            area.height - status_height,
            visual_rows,
            visual_rows.min(2),
            menu_height,
        );
        let [transcript, completion, composer, status] = Layout::vertical([
            Constraint::Length(transcript_height),
            Constraint::Length(menu_height),
            Constraint::Length(composer_height),
            Constraint::Length(status_height),
        ])
        .areas(area);
        let snap = render_messages(frame, app, transcript);
        if let Some(menu) = menu {
            render_completion_menu(frame, &menu, completion, app.lang);
        }
        let rows = render_input_from_layout(frame, app, &layout, composer);
        render_status(frame, app, status);
        (snap, Some(rows))
    };
    app.set_transcript_snapshot(snapshot);
    app.composer_scroll = composer_rows.map_or(layout.first_row, |(_, first_row)| first_row);
    app.composer =
        composer_rows.map(|rows| composer_snapshot(rows, inner_width as usize, composer_text));
}

/// Assign rows to the transcript, completion menu, and composer after
/// reserving the active footer. Keep an editor or preview row, the selected
/// completion, and one transcript row before adding composer borders.
fn body_row_heights(
    body_height: u16,
    draft_rows: u16,
    minimum_draft_rows: u16,
    menu_height: u16,
) -> [u16; 3] {
    let menu_min = u16::from(menu_height > 0);
    let composer_min = body_height
        .saturating_sub(menu_min + 1)
        .clamp(body_height.min(minimum_draft_rows), 3);
    let transcript_min = (body_height - composer_min).saturating_sub(menu_min).min(5);
    let menu_height = menu_height.min(body_height - transcript_min - composer_min);
    let composer_height = (draft_rows + 2).min(body_height - transcript_min - menu_height);
    [
        body_height - menu_height - composer_height,
        menu_height,
        composer_height,
    ]
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
                &crate::cli::program_name(),
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

fn status_badges(app: &App) -> Vec<Span<'static>> {
    use deep_code_agent::PermissionMode;

    // A permanent chip so the current permission mode is always visible.
    // Yolo shouts (red bold); the rest are quiet.
    let mode = app.permission_mode();
    let chip_style = match mode {
        PermissionMode::Yolo => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        PermissionMode::AcceptEdits => Style::default().fg(Color::Cyan),
        PermissionMode::Default => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![Span::styled(
        format!("[{}] ", crate::app::perm_mode_label(app.lang, mode)),
        chip_style,
    )];
    // Unsandboxed shouts louder than any permission mode: `[sandbox] mode =
    // "off"` removes the boundary every other sentence in the status line and
    // every approval panel is predicated on, so a session running bare must be
    // unmistakable at a glance.
    if app.sandbox_off {
        spans.push(Span::styled(
            "[unsandboxed] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

fn render_status(frame: &mut Frame<'_>, app: &mut App, area: ratatui::layout::Rect) {
    let badges = status_badges(app);
    if app.yolo_confirmation_active() {
        let size = frame.area().as_size();
        let hint = app.yolo_confirmation.hint(size);
        let visible = render_yolo_confirmation(frame, area, badges, app.lang, hint);
        app.yolo_confirmation.set_visible(visible.then_some(size));
        return;
    }
    app.yolo_confirmation.set_visible(None);

    // An open Ctrl+R search owns the row while it lasts, ahead of an error or
    // a streaming turn: it takes the keys — Esc included — so a search that
    // is not on screen turned the user's Esc on a running turn into closing
    // something they could not see.
    if app.history_search_open() {
        render_history_search_field(frame, area, badges, app);
        return;
    }
    if let Some(error) = &app.error {
        render_error_status(frame, area, badges, app.lang, error);
    } else if app.is_streaming {
        let (activity, clock) = app
            .streaming_activity()
            .unwrap_or_else(|| (app.status_line(), String::new()));
        render_streaming_status(
            frame,
            area,
            badges,
            app,
            (&activity, &clock),
            empty_composer_accepts_hint(app),
        );
    } else {
        render_idle_status(frame, area, badges, app);
    }
}

fn render_yolo_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    badges: Vec<Span<'_>>,
    lang: Lang,
    hint: Option<TextId>,
) -> bool {
    if area.is_empty() {
        return false;
    }
    let badges = Line::from(badges);
    let (fits, rows) = yolo_confirmation_layout(&badges, area, lang);
    let buffer = frame.buffer_mut();
    let (x, y) = if rows == 1 {
        buffer.set_line(area.x, area.y, &badges, area.width)
    } else {
        let hint_y = area.bottom() - 1;
        if rows == 2 && area.height > 1 {
            buffer.set_line(area.x, area.y, &badges, area.width);
        } else {
            // Keep the sandbox badge first in priority when the pane is too
            // short to give each badge a row above the resize hint.
            for (row, badge) in (area.y..hint_y).rev().zip(badges.spans.iter().rev()) {
                buffer.set_span(area.x, row, badge, area.width);
            }
        }
        (area.x, hint_y)
    };
    let style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let text = if fits {
        tr(lang, TextId::PermModeYoloWarning)
    } else {
        tr(lang, TextId::PermModeYoloResize)
    };
    let (x, _) = buffer.set_stringn(x, y, text, usize::from(area.right() - x), style);
    if fits && let Some(hint) = hint {
        buffer.set_stringn(x, y, tr(lang, hint), usize::from(area.right() - x), style);
    }
    fits
}

/// Whether the full prompt fits and how many footer rows it needs.
fn yolo_confirmation_layout(badges: &Line<'_>, area: Rect, lang: Lang) -> (bool, u16) {
    let width = usize::from(area.width);
    let available = width.saturating_sub(badges.width());
    let warning = status_text_width(tr(lang, TextId::PermModeYoloWarning));
    let hint = status_text_width(tr(lang, TextId::PermModeYoloConfirm))
        .max(status_text_width(tr(lang, TextId::PermModeYoloRetry)))
        .max(status_text_width(tr(lang, TextId::PermModeYoloFinish)));
    let prompt_width = warning + hint;
    if prompt_width <= available {
        return (true, 1);
    }
    let badge_rows = if badges.width().saturating_sub(1) <= width {
        1
    } else {
        badges.spans.len() as u16
    };
    let separate_rows = badge_rows + 1;
    if prompt_width <= width && area.height >= separate_rows {
        return (true, separate_rows);
    }
    if status_text_width(tr(lang, TextId::PermModeYoloResize)) <= available {
        return (false, 1);
    }
    (false, separate_rows)
}

fn render_error_status(
    frame: &mut Frame<'_>,
    area: Rect,
    mut badges: Vec<Span<'_>>,
    lang: Lang,
    error: &str,
) {
    if area.is_empty() {
        return;
    }
    badges.push(Span::styled(
        tr(lang, TextId::ErrorPrefix),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ));
    let buffer = frame.buffer_mut();
    let (x, y) = buffer.set_line(area.x, area.y, &Line::from(badges), area.width);
    let available = usize::from(area.right() - x);
    // Runtime errors can quote paths and commands supplied by the model.
    let error = neutralize_display_text(error);
    buffer.set_stringn(x, y, error, available, Color::DarkGray);
}

fn status_text_width(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

fn render_idle_status(frame: &mut Frame<'_>, area: Rect, badges: Vec<Span<'_>>, app: &App) {
    if area.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    let (x, y) = buffer.set_line(area.x, area.y, &Line::from(badges), area.width);
    let available = usize::from(area.right() - x);
    if available == 0 {
        return;
    }
    let [model, reasoning, context, note] = app
        .status_line_parts()
        .map(|part| neutralize_display_text(&part));
    let model_width = status_text_width(&model);
    let reasoning_width = status_text_width(&reasoning);
    let context_width = status_text_width(&context);
    let full_details_width = model_width + reasoning_width + context_width;
    let note_width = status_text_width(&note);
    // The rollback hint is only useful as a complete command and checkpoint
    // ID. Keep the model and context fields visible while it has no room.
    let (note, note_width) = if note_width > 0
        && full_details_width + note_width > available
        && app.rollback_status_hint().as_deref() == Some(app.status.as_str())
    {
        ("", 0)
    } else {
        (note.as_str(), note_width)
    };
    if note_width >= available {
        let note = note.trim_start();
        if status_text_width(note) > available
            && let Some((key, warning)) = &app.quit_armed
            && app.status == *warning
        {
            let key = match key {
                QuitKey::Esc => "Esc",
                QuitKey::CtrlC => "Ctrl+C",
            };
            let short = tr_with(app.lang, TextId::StatusQuitConfirmShort, &[("key", key)]);
            buffer.set_stringn(x, y, short, available, Color::DarkGray);
        } else {
            buffer.set_stringn(x, y, note, available, Color::DarkGray);
        }
        return;
    }

    // Keep the complete model and percentage when they fit together, then
    // add the whole reasoning field if there is space.
    let mut details_width = available - note_width;
    let context = if model_width + context_width <= details_width {
        details_width -= context_width;
        context.as_str()
    } else {
        ""
    };
    let reasoning = if model_width + reasoning_width <= details_width {
        reasoning.as_str()
    } else {
        ""
    };
    let (mut x, _) = buffer.set_stringn(x, y, model, details_width, Color::DarkGray);
    for field in [reasoning, context, note] {
        (x, _) = buffer.set_stringn(x, y, field, usize::from(area.right() - x), Color::DarkGray);
    }
}

fn render_streaming_status(
    frame: &mut Frame<'_>,
    area: Rect,
    badges: Vec<Span<'_>>,
    app: &App,
    (activity, clock): (&str, &str),
    show_composer_hint: bool,
) {
    if area.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    let (x, y) = buffer.set_line(area.x, area.y, &Line::from(badges), area.width);
    let available = usize::from(area.right() - x);
    let activity = neutralize_display_text(activity);
    let clock = neutralize_display_text(clock);
    let clock_width = clock.width();
    let mut activity_widths = activity.graphemes(true).map(UnicodeWidthStr::width);
    let first_activity_width = activity_widths.find(|&width| width > 0).unwrap_or_default();
    let activity_label_width = first_activity_width + activity_widths.sum::<usize>();
    let full_activity_width = activity_label_width + clock_width;
    let full_cancel = tr(app.lang, TextId::StatusEscCancel);
    let short_cancel = "Esc";
    let count = app.steering_queue.len().to_string();
    let args = [("count", count.as_str())];
    let preferred_gap = 3;
    let fits = |activity_width, cancel_width, hint_width| {
        let suffix_width =
            cancel_width + hint_width + usize::from(cancel_width > 0 && hint_width > 0);
        activity_width + suffix_width + usize::from(activity_width > 0 && suffix_width > 0)
            <= available
    };

    // Reserve the count, spinner, clock, and cancel key together before
    // spending any of their columns on longer labels. The transcript marks
    // its pending entries with the same compact count symbol.
    let compact_count = (!app.steering_queue.is_empty())
        .then(|| format!("◷{count}"))
        .filter(|value| value.width() <= available);
    let count_width = compact_count.as_deref().map_or(0, UnicodeWidthStr::width);
    let activity_minimum = if fits(first_activity_width, 0, count_width) {
        first_activity_width
    } else {
        0
    };
    let show_clock = activity_minimum > 0
        && clock_width > 0
        && fits(activity_minimum + clock_width, 0, count_width);
    let reserved_clock_width = if show_clock { clock_width } else { 0 };
    let activity_minimum = activity_minimum + reserved_clock_width;
    let short_cancel = if fits(activity_minimum, short_cancel.width(), count_width) {
        short_cancel
    } else {
        ""
    };
    let full_hint = show_composer_hint
        .then(|| tr_with(app.lang, TextId::ComposerPlaceholderSteering, &args))
        .filter(|hint| {
            full_activity_width + full_cancel.width() + hint.width() + 2 * preferred_gap
                <= available
        });
    let (hint, hint_gap) = if let Some(hint) = full_hint {
        (hint, preferred_gap)
    } else if let Some(compact_count) = compact_count {
        let queued = tr_with(
            app.lang,
            TextId::ComposerPlaceholderSteeringQueuedShort,
            &args,
        );
        let hint = [queued, format!("◷ {count}")]
            .into_iter()
            .find(|label| fits(activity_minimum, short_cancel.width(), label.width()))
            .unwrap_or(compact_count);
        (hint, 1)
    } else {
        let short = tr(app.lang, TextId::ComposerPlaceholderSteeringShort);
        if show_composer_hint
            && app.steering_queue.is_empty()
            && !short_cancel.is_empty()
            && fits(full_activity_width, short_cancel.width(), short.width())
        {
            (short.into(), 1)
        } else {
            (String::new(), 0)
        }
    };
    let cancel = if fits(
        full_activity_width,
        full_cancel.width(),
        hint.width() + hint_gap.saturating_sub(1),
    ) {
        full_cancel
    } else {
        short_cancel
    };
    let suffix = if cancel.is_empty() {
        hint
    } else {
        format!("{cancel}{:hint_gap$}{hint}", "")
    };
    let activity_space = available.saturating_sub(suffix.width());
    let full_activity_fits = full_activity_width < activity_space;
    let gap = if suffix.is_empty() || activity_minimum == 0 {
        0
    } else if reserved_clock_width > 0 && !full_activity_fits {
        1
    } else {
        let reserve = if full_activity_fits {
            full_activity_width
        } else {
            activity_minimum
        };
        preferred_gap.min(activity_space.saturating_sub(reserve))
    };
    let label_budget = if activity_minimum > 0 {
        activity_space.saturating_sub(gap + reserved_clock_width)
    } else {
        0
    };
    let ellipsis =
        show_clock && activity_label_width > label_budget && label_budget > first_activity_width;
    let (mut activity_end, _) = buffer.set_stringn(
        x,
        y,
        activity,
        label_budget - usize::from(ellipsis),
        Color::Cyan,
    );
    if ellipsis {
        (activity_end, _) = buffer.set_stringn(activity_end, y, "…", 1, Color::Cyan);
    }
    if show_clock {
        (activity_end, _) = buffer.set_stringn(activity_end, y, clock, clock_width, Color::Cyan);
    }
    let suffix_x = if activity_end == x {
        x
    } else {
        activity_end + gap as u16
    };
    buffer.set_stringn(
        suffix_x,
        y,
        &suffix,
        usize::from(area.right() - suffix_x),
        Color::DarkGray,
    );
}

fn render_history_search_field(
    frame: &mut Frame<'_>,
    area: Rect,
    badges: Vec<Span<'_>>,
    app: &mut App,
) {
    if area.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    let (status_x, y) = buffer.set_line(area.x, area.y, &Line::from(badges), area.width);
    let available = usize::from(area.right() - status_x);
    if available == 0 {
        return;
    }

    let focused = app.history_search_focused();
    let Some((label, query, suffix)) = app.history_search_display(available) else {
        return;
    };
    let (query_x, _) = buffer.set_stringn(status_x, y, label, available - 1, Color::Yellow);
    let query_budget = usize::from(area.right() - query_x) - 1;
    let (cursor_x, _) = buffer.set_stringn(query_x, y, query, query_budget, Color::Yellow);
    let result_width = status_text_width(suffix);
    let result_budget = usize::from(area.right() - cursor_x);
    let result_end = if result_width <= result_budget {
        buffer
            .set_stringn(cursor_x, y, suffix, result_budget, Color::Yellow)
            .0
    } else {
        cursor_x
    };
    if focused {
        if result_width > 0
            && usize::from(result_end - cursor_x) == result_width
            && let Some(hints) = app.history_search_hints(usize::from(area.right() - result_end))
        {
            buffer.set_string(result_end, y, hints, Color::Yellow);
        }
        frame.set_cursor_position((cursor_x, y));
    }
}

#[cfg(test)]
mod tests;
