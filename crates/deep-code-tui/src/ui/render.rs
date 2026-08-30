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

/// Max completion rows shown at once; the list windows around the selection so
/// wrapping past the top/bottom keeps the highlighted item on screen.
const COMPLETION_VISIBLE_ROWS: usize = 8;

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

fn render_completion_menu(
    frame: &mut Frame<'_>,
    menu: &crate::app::CompletionMenu,
    area: ratatui::layout::Rect,
    lang: Lang,
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
            // `value` is a workspace FILENAME (`@` completion reads
            // `list_workspace_files`), so it is whatever is on disk — a cloned
            // repo or a model write can name a file `evil\x1b[8mhidden.txt`.
            // `Paragraph` filters zero-width symbols but not `\x1b`, which
            // measures 1, so an unsanitized menu row flushes the escape to the
            // terminal; SGR conceal turned on here survives into later frames,
            // including an approval panel. Nothing else guards this path.
            let value_span = Span::raw(neutralize_display_text(value));
            if index == menu.selected {
                spans.push(value_span.bold());
            } else {
                spans.push(value_span);
            }
            if !hint.is_empty() {
                // Sanitized for the same reason as `value` above, not because
                // today's hints are hostile: they are i18n for slash items and
                // empty for file items. An unfiltered span sitting beside a
                // filtered one is how the last several of these got in — the
                // asymmetry reads as a deliberate exemption.
                spans.push(Span::styled(
                    format!("  {}", neutralize_display_text(hint)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let panel = Paragraph::new(lines).block(
        Block::default()
            .title(tr(lang, TextId::CompletionMenuTitle))
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
    // No transcript border/title — a 1-col left gutter and the input box
    // below provide all the structure, keeping every column for content.
    let viewport = usize::from(area.height).max(1);
    let content_width = area.width.saturating_sub(2).max(8);

    // Render the WHOLE transcript into a stable line buffer: a fixed
    // coordinate space is what lets mouse drag-selection map cleanly, and
    // bottom-anchored scroll then just windows it.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for cell in &app.history {
        lines.extend(cell_lines(cell, content_width, app.lang));
    }
    let preview = app
        .active_turn
        .as_ref()
        .map(|active| active.preview_cells())
        .unwrap_or_default();
    for cell in &preview {
        lines.extend(cell_lines(cell, content_width, app.lang));
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

/// Render one transcript cell: speakers are distinguished by a coloured
/// marker glyph rather than a text label, and there is no per-cell box.
/// Secondary content (reasoning, tool noise, system) is dimmed; the
/// user line and assistant prose carry the conversation.
///
/// Assistant text always renders as markdown — including while still
/// streaming — so formatting is consistent throughout. `parse_blocks` treats
/// an unclosed code fence as a code block, so a half-streamed fence renders
/// without flicker; a pipe-table header stays plain text until its separator
/// row arrives — the same no-flicker rule from the other direction.
fn cell_lines(cell: &HistoryCell, width: u16, lang: Lang) -> Vec<Line<'static>> {
    let mut lines = cell_lines_unsanitized(cell, width, lang);
    // One choke point for the whole transcript, applied to the finished lines
    // rather than to each variant's inputs: wrapping has already consumed the
    // real newlines by now, so anything control-shaped left in a span is
    // something the model put there, and a new `HistoryCell` variant cannot
    // forget to opt in.
    //
    // ratatui carries an escape byte into a cell verbatim, and `unicode-width`
    // reports `\x1b` as width 1, so `Paragraph`'s zero-width filter does not
    // drop it. That made ordinary assistant prose an attack on the approval
    // panel drawn in the same frame: `\x1b[8m` turns on SGR conceal, and since
    // ratatui only emits `NoHidden` when its OWN tracked modifier had HIDDEN,
    // nothing ever turns it back off — every cell flushed afterwards,
    // including the whole prompt below, is invisible. `\x1b[12;3H` is worse
    // still: it repositions the cursor and paints attacker text at a chosen
    // row, which is how a counterfeit "Grant target (resolved): /tmp/harmless"
    // can appear inside a security prompt that never rendered it.
    //
    // The bidi/zero-width family is stripped here too, and OWNED here: none
    // of [`BIDI_AND_ZERO_WIDTH`] is `char::is_control`, and this defense used
    // to lean on measured, undocumented ratatui 0.29 behavior (the family was
    // dropped during line composition) — load-bearing for the approval panel,
    // since a bidi override can reorder what the user reads in the same
    // frame, yet one dependency bump away from vanishing. ZWNJ/ZWJ and the
    // variation selectors are deliberately NOT stripped — legitimate joiners
    // in emoji and e.g. Persian text — and stay safe by a measured invariant
    // instead: they ride inside the preceding grapheme cluster's cell, never
    // a column of their own.
    //
    // Pinned by `transcript_text_cannot_carry_an_escape_into_a_cell`,
    // `zero_width_code_points_cannot_reorder_or_pad_the_frame` (the ratatui
    // tripwire) and `neutralize_strips_every_invisible_code_point` (the one
    // that fails if OUR strip is removed — the frame-level test cannot, since
    // ratatui drops the family on its own and so cannot tell the two apart).
    for line in &mut lines {
        for span in &mut line.spans {
            if span
                .content
                .chars()
                .any(|ch| ch.is_control() || is_bidi_or_zero_width(ch))
            {
                span.content = neutralize_transcript_text(&span.content).into();
            }
        }
    }
    lines
}

/// Model text on its way to the OS clipboard.
///
/// The invisible-family deletion is shared with the display sanitizers, but
/// the control rule has to differ: `\n` and `\t` ARE the document here, not
/// stray bytes on a rendered row, so mapping them to spaces — correct for a
/// single line of a panel — would flatten every code block being copied.
/// Everything else control-shaped becomes a space: `\x1b` above all, and `\r`,
/// which can make a paste into a shell submit itself.
///
/// Drag-select copy was already safe (it reads the sanitized frame lines), so
/// `/copy` reaching for the raw cell text was the two copy paths in one app
/// disagreeing — a wiring gap, not a missing capability.
pub(crate) fn sanitize_for_clipboard(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' {
            out.push(ch);
        } else {
            neutralize_char_into(&mut out, ch);
        }
    }
    out
}

/// Control and bidi/zero-width characters out of a finished transcript span.
///
/// One exception on top of [`neutralize_char_into`]: a tab becomes four
/// spaces rather than one, because code blocks reach here tab-indented and
/// collapsing that to a single column misreads the code. That is the one
/// place this function does NOT preserve the wrap step's column count —
/// `UnicodeWidthStr::width("\t")` is 1 (it is `UnicodeWidthChar` that returns
/// `None`, and the wrap step measures graphemes through the `Str` form), so
/// each tab adds three columns beyond the budget, not four. ratatui truncates
/// the overflow rather than bleeding into a neighbouring widget, so a deeply
/// tab-indented line loses its tail; that is the accepted trade for readable
/// indentation.
fn neutralize_transcript_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\t' {
            out.push_str("    ");
        } else {
            neutralize_char_into(&mut out, ch);
        }
    }
    out
}

fn cell_lines_unsanitized(cell: &HistoryCell, width: u16, lang: Lang) -> Vec<Line<'static>> {
    let width = width as usize;
    let dim = Style::default().fg(Color::DarkGray);
    match cell {
        HistoryCell::Welcome {
            version,
            model,
            reasoning,
            offline,
            workspace,
            resumed_turns,
            persistent,
        } => {
            let cyan = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            // Pad labels to a fixed char width so the values align per language.
            let label = |id: TextId| Span::styled(format!("{:<7} ", tr(lang, id)), dim);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("deep-code", cyan),
                    Span::styled(format!("  v{version}"), dim),
                ]),
                Line::from(Span::styled("─".repeat(width.clamp(8, 46)), dim)),
            ];
            if *offline {
                lines.push(Line::from(vec![
                    label(TextId::WelcomeStatusLabel),
                    Span::styled(
                        tr(lang, TextId::WelcomeOffline),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    label(TextId::WelcomeModelLabel),
                    Span::raw(tr_with(
                        lang,
                        TextId::WelcomeModelValue,
                        &[("model", model), ("reasoning", reasoning)],
                    )),
                ]));
            }
            lines.push(Line::from(vec![
                label(TextId::WelcomeWorkspaceLabel),
                Span::raw(left_truncate(workspace, width.saturating_sub(8).max(8))),
            ]));
            lines.push(Line::from(vec![
                label(TextId::WelcomeSessionLabel),
                Span::raw(crate::history::session_summary(
                    lang,
                    *resumed_turns,
                    *persistent,
                )),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                tr(lang, TextId::WelcomeIntro),
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
            let text = cell.lines(lang).join(" ");
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
        // Live output of a running tool: dim, indented under the call line,
        // no trailing blank (the block keeps growing while streaming).
        HistoryCell::ToolStream { text } => text
            .lines()
            .flat_map(|logical| wrap_prefixed("    ", logical, width, dim, dim))
            .collect(),
        // Diagnostics / Checkpoint / Compaction / System: dim secondary lines.
        _ => {
            let mut lines = Vec::new();
            for logical in cell.lines(lang) {
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

/// Risk tier (Debug of `RiskLevel`) → (localized tag, accent colour). Risk is
/// shown as colour, not a `Risk: …` field. Unknown tiers fall back to amber.
fn risk_display(risk: &str, lang: Lang) -> (&'static str, Color) {
    match risk {
        "High" => (tr(lang, TextId::RiskHigh), Color::Red),
        "Medium" => (tr(lang, TextId::RiskMedium), Color::Yellow),
        "Low" => (tr(lang, TextId::RiskLow), Color::DarkGray),
        _ => ("", Color::Yellow),
    }
}

/// The human-meaningful action behind a tool call — the shell command, the file
/// path, etc. — instead of the raw JSON blob. Falls back to compact arguments.
///
/// `tool_name` decides which key may occupy the line, rather than letting the
/// first familiar key win for every tool. It matters for
/// `request_write_root`, whose subject is unambiguously `path`: the generic
/// scan puts `command` ahead of `path`, so an extra key would render
/// attacker-chosen text on the action line of a boundary prompt. The runtime
/// now refuses such an argument set outright, and pinning the key here means
/// the panel does not depend on that refusal to show the right subject.
fn extract_action(tool_name: &str, arguments_json: &str) -> String {
    let keys: &[&str] = if tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL {
        &["path"]
    } else {
        &["command", "path", "file_path", "url", "pattern", "query"]
    };
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments_json)
        && let Some(object) = value.as_object()
    {
        for key in keys {
            if let Some(text) = object.get(*key).and_then(serde_json::Value::as_str) {
                return crate::history::collapse_whitespace(text);
            }
        }
    }
    crate::history::collapse_whitespace(arguments_json)
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

/// The composer's own variant: same rule, but **one char in, one char out**,
/// and `'\n'` passes through.
///
/// The newline exemption is not a hole, it is the layout contract. `'\n'` is
/// `is_control()`, so mapping it to a space made `wrap_input_lines`'
/// `split('\n')` and `cursor_row_col`'s `"\n"` branch — both of which read
/// THIS string — dead code, and a multi-line draft collapsed onto one row: the
/// box stopped growing, and `↑`/`↓` (which navigate the raw `app.input`) moved
/// the caret by a line model the screen no longer showed. Alt+Enter and Ctrl+J
/// insert a literal newline and are advertised in `HelpKeys` in both locales.
/// It still cannot reach a cell: `wrap_input_lines` consumes it before any
/// drawing happens, and `Buffer::set_stringn` would drop it regardless.
///
/// The two sibling sanitizers each carry their own exemption list for the same
/// kind of reason — [`neutralize_transcript_text`] keeps `'\t'`,
/// [`sanitize_for_clipboard`] keeps `'\n'` and `'\t'` — so read the three
/// together before adding a fourth.
///
/// The composer is the one surface whose string is also the thing that gets
/// SENT, and `app.input_cursor` is a char index into it — so the buffer itself
/// must stay verbatim (an `@`-completion has to name the file that really
/// exists) and the sanitizing has to be length-preserving, or the cursor ends
/// up pointing past the text it sits in. Substituting instead of deleting
/// satisfies both: index arithmetic is untouched, and `layout_input` derives
/// the wrapped lines AND the cursor position from this same returned string, so
/// they cannot disagree.
///
/// Worth stating why the surface needed covering at all: it renders through
/// `Buffer::set_string`, whose own filter (`!symbol.contains(char::is_control)`
/// plus a width-0 skip) is undocumented ratatui behaviour — the very lease this
/// module exists to stop renting — and which in any case passes U+2028, the
/// Hangul fillers and the annotation trio straight into a cell. The completion
/// menu drew its rows sanitized while pushing the raw directory entry in here,
/// so what the user saw and what they inserted were different strings.
pub(crate) fn neutralize_composer_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch == '\n' {
                ch
            } else if ch.is_control() || is_bidi_or_zero_width(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

/// Model-influenced text about to become an approval panel line: control
/// characters become spaces and the invisible reordering/padding family is
/// deleted (see [`neutralize_display_text`]), and the width is capped. Every
/// free-text argument of [`approval_lines`] passes through one of the two,
/// except `risk`: [`risk_display`] maps it to a `&'static str`, so that one
/// cannot echo its input at all. Pinned by
/// `approval_lines_sanitize_every_text_field`, which asserts one marker per
/// field — with both an escape sequence and a bidi override in every marker,
/// so a field that loses either half names itself — and renders the
/// root-grant branch as well, or the fields gated behind it would be passed
/// in and never drawn, pinning nothing.
///
/// The cap is in terminal **columns**, not characters: the panel reserves rows
/// by measuring its own wrapped body, so a cap the layout cannot convert to
/// rows is not a bound at all. Capping 240 *characters* let 240 CJK characters
/// claim 480 columns — seven rows at an 80-column terminal — which is how
/// model-supplied text could still push the resolved grant target past the
/// bottom edge after the height itself had been made content-sized.
fn sanitize_panel_text(text: &str, max_cols: usize) -> String {
    crate::history::truncate_display_width(neutralize_display_text(text).trim(), max_cols)
}

/// The decision-critical head of a `request_write_root` panel: the boundary
/// caution, the directory the grant would ACTUALLY land on, a symlink warning
/// when the spelling resolves elsewhere, and — last, and labelled as such —
/// the model's own spelling.
///
/// Split out so the panel can render it as a PINNED block, outside the
/// scrollable region. Scrolling used to carry it away: `End` (bound for
/// reading a long justification) clamps to the bottom of the body, which put
/// the resolved target above the viewport with no "more above" marker and the
/// panel still armed — the same "approve a directory you were never shown"
/// the content-sized panel was meant to end, reached by a keystroke instead of
/// a small terminal.
///
/// One source of truth: `approval_lines` extends with exactly this, so the
/// count the panel pins cannot drift from what it draws.
fn root_grant_lines(
    resolved_target: Option<&str>,
    action: &str,
    arguments_json: &str,
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    let caution = Style::default().fg(Color::Yellow);
    lines.extend(wrap_prefixed(
        "  ",
        tr(lang, TextId::ApprovalRootGrant),
        width,
        caution,
        caution,
    ));
    match resolved_target {
        Some(target) => {
            let target_style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            // Sanitized like every panel line. The runtime already
            // refuses targets with control characters in the name, so
            // this is the defense-in-depth layer, not the only one.
            let shown = sanitize_panel_text(target, 240);
            lines.extend(wrap_prefixed(
                "  ",
                &tr_with(lang, TextId::ApprovalRootGrantTarget, &[("path", &shown)]),
                width,
                target_style,
                target_style,
            ));
            // The request resolves somewhere its spelling doesn't say
            // (symlink in it): call that out, or an innocuous-looking
            // spelling could pass for the real target. Compared by path
            // components, so a benign respelling — trailing slash, `.`
            // segments — is not accused of resolving elsewhere.
            let requested = serde_json::from_str::<serde_json::Value>(arguments_json)
                .ok()
                .and_then(|arguments| {
                    arguments
                        .get("path")
                        .and_then(|value| value.as_str().map(|path| path.trim().to_string()))
                });
            let resolves_elsewhere = requested.as_deref().is_none_or(|raw| {
                std::path::Path::new(raw)
                    .components()
                    .ne(std::path::Path::new(target).components())
            });
            if resolves_elsewhere {
                lines.extend(wrap_prefixed(
                    "  ",
                    tr(lang, TextId::ApprovalRootGrantSymlink),
                    width,
                    caution,
                    caution,
                ));
            }
        }
        // Defensive: with prompt-time triage a root grant is only parked
        // WITH a resolved target; still, never render a boundary prompt
        // that silently lacks the one line that matters.
        None => lines.extend(wrap_prefixed(
            "  ",
            tr(lang, TextId::ApprovalRootGrantUnresolved),
            width,
            caution,
            caution,
        )),
    }
    // The model's own spelling comes last and says so: it is what was
    // asked for, not what approving would grant.
    lines.extend(wrap_prefixed(
        "  ",
        &tr_with(
            lang,
            TextId::ApprovalRootGrantRequested,
            &[("path", action)],
        ),
        width,
        dim,
        dim,
    ));
    lines
}

/// The decision-critical head of ANY approval panel: the header, and the
/// subject the decision is about — for a root grant the resolved-directory
/// block, for every other tool the action line (the command, the file being
/// written).
///
/// Rendered by the panel as a PINNED block, outside the scrollable region, so
/// that "armed" can mean "the human can see what they are deciding about" for
/// every tool rather than only for root grants.
///
/// Pinning the head used to cover the root grant alone, and the generic arming
/// condition — "at least one body row was painted" — was left as a proxy for
/// the real invariant. It only ever held for the pinned case: body row 0 is the
/// header, which names the tool but never the action, so on a 5- or 6-row
/// terminal a `shell` or `write_file` prompt armed with its subject one row
/// below the edge (and at 5 rows the overflow indicator, splitting `[Min(1),
/// Length(1)]` over a single row, got zero rows and vanished too). Focus starts
/// on Approve for those, so one `y` ran a command that was never displayed.
///
/// One source of truth: [`approval_lines`] *is* this function plus the
/// scrollable remainder, so the count the panel pins cannot drift from what it
/// draws. The previous split recomputed the count from an unsanitized action
/// while the body used the capped one, which over-counted and let the pinned
/// block swallow the whole body.
fn approval_head_lines(
    tool_name: &str,
    risk: &str,
    action: &str,
    resolved_target: Option<&str>,
    arguments_json: &str,
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let (risk_tag, risk_color) = risk_display(risk, lang);
    let risk_style = Style::default().fg(risk_color);

    let mut header = vec![
        Span::styled("● ", risk_style),
        Span::styled(
            tr(lang, TextId::ApprovalNeeded),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", dim),
        Span::styled(
            sanitize_panel_text(tool_name, 120),
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

    // A root grant changes the boundary itself, not just this one run —
    // called out in warning color, together with the directory the grant would
    // ACTUALLY land on: the runtime resolves the request once for this prompt
    // and later refuses the grant unless it still resolves identically, so
    // this line — not the model's raw spelling in the arguments — is what the
    // human is judging.
    //
    // These lines come BEFORE the requested spelling, and the spelling is
    // rendered labelled rather than as a bare action line. Both facts are
    // load-bearing. The spelling is model-controlled and of model-chosen
    // width, so as the first body element it could push the resolved target
    // off the bottom of a content-sized panel; and a bare, unlabelled action
    // line wraps into continuation rows that are indistinguishable from a
    // field row, which let a spelling ending in `.../Grant target (resolved):
    // /tmp/safe` paint a counterfeit target above the real one. Putting the
    // resolved directory first and labelling the spelling removes both.
    if tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL {
        lines.extend(root_grant_lines(
            resolved_target,
            action,
            arguments_json,
            width,
            lang,
        ));
    } else {
        lines.extend(wrap_prefixed(
            "  ",
            action,
            width,
            Style::default(),
            Style::default(),
        ));
    }
    lines
}

/// Minimal, borderless approval block matching the welcome/picker style: a
/// risk-coloured `●` + tool, the action it will take (prominent), an optional
/// dim description, and only meaningful metadata (sandbox / matched rule).
///
/// Starts with [`approval_head_lines`], which the panel pins: this function is
/// that head plus the scrollable remainder, which is what makes the pinned row
/// count impossible to drift from what is drawn.
#[allow(clippy::too_many_arguments)]
fn approval_lines(
    tool_name: &str,
    risk: &str,
    requires_sandbox: bool,
    network: bool,
    justification: Option<&str>,
    resolved_target: Option<&str>,
    matched_rule: Option<&str>,
    description: &str,
    arguments_json: &str,
    preview: Option<&str>,
    safety_notes: &[SafetyNote],
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let action = sanitize_panel_text(&extract_action(tool_name, arguments_json), 240);
    let mut lines = approval_head_lines(
        tool_name,
        risk,
        &action,
        resolved_target,
        arguments_json,
        width,
        lang,
    );

    // Neutralised but not capped: tool descriptions run past any line cap, and
    // they are written by this crate, not the model — the filter is uniformity
    // with the rest of the panel, not a boundary of its own.
    let description = neutralize_display_text(description);
    let description = description.trim();
    if !description.is_empty() && description != action {
        lines.extend(wrap_prefixed("  ", description, width, dim, dim));
    }

    // The model's own words, clearly labelled as its claim (it wrote this
    // text; approving is still entirely the human's judgement).
    if let Some(text) = justification {
        let clean = sanitize_panel_text(text, 240);
        if !clean.is_empty() {
            let claim = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            lines.extend(wrap_prefixed(
                "  ",
                &tr_with(lang, TextId::ApprovalJustification, &[("text", &clean)]),
                width,
                claim,
                claim,
            ));
        }
    }

    let mut meta = Vec::new();
    // The network ask leads: it is what makes this approval different from an
    // ordinary run of the same command.
    if network {
        meta.push(tr(lang, TextId::ApprovalNetwork).to_string());
    }
    if requires_sandbox {
        // `requires_sandbox` is what the *policy* asked for. Whether the host can
        // deliver it is a separate question — the Windows Job Object confines
        // neither writes nor network, so claiming "sandboxed execution" there
        // would tell the user they are protected while they approve a command
        // that is not. Say which it is.
        //
        // Three answers, not two: a host can also confine everything except a
        // right its kernel is too old to express (Landlock before 6.2 does not
        // govern `truncate(2)`). Rounding that up to "sandboxed" is the same
        // overclaim in a quieter form, and rounding it down to "no sandbox"
        // would push users off a boundary that is holding.
        let text = match deep_code_agent::sandbox_enforcement() {
            deep_code_agent::Enforcement::Full => tr(lang, TextId::ApprovalSandbox).to_string(),
            // Names the binary the user actually invoked. This string used to
            // hardcode `deepcode`, which is only the npm spelling — a source
            // build installs `deep-code`, so the one actionable step in a
            // security-path message was a command those users do not have.
            deep_code_agent::Enforcement::Partial { .. } => tr_with(
                lang,
                TextId::ApprovalPartialSandbox,
                &[("program", &crate::cli::program_name())],
            ),
            deep_code_agent::Enforcement::None => tr(lang, TextId::ApprovalNoSandbox).to_string(),
        };
        meta.push(text);
    }
    if let Some(rule) = matched_rule {
        let rule = sanitize_panel_text(rule, 120);
        meta.push(tr_with(lang, TextId::ApprovalRule, &[("rule", &rule)]));
    }
    if !meta.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", meta.join(" · ")),
            dim,
        )));
    }

    // Advisory static notes for shell commands: why this warrants review and a
    // paired suggestion. Not a dry-run — just a heads-up before the user acts.
    if !safety_notes.is_empty() {
        let caution = Style::default().fg(Color::Yellow);
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("  {}", tr(lang, TextId::ApprovalCautionHeader)),
            caution,
        )));
        for note in safety_notes {
            lines.extend(wrap_prefixed(
                "  • ",
                tr(lang, note.reason),
                width,
                caution,
                caution,
            ));
            lines.extend(wrap_prefixed(
                "    ↳ ",
                tr(lang, note.suggestion),
                width,
                dim,
                dim,
            ));
        }
    }

    if let Some(preview) = preview.filter(|preview| !preview.trim().is_empty()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("  {}", tr(lang, TextId::ApprovalPreviewHeader)),
            dim,
        )));
        let added = Style::default().fg(Color::Green);
        let removed = Style::default().fg(Color::Red);
        for raw in preview.lines() {
            // The widest model-controlled text on the panel: a write_file diff
            // is that call's own `content` argument (apply_patch's `old`/`new`)
            // rendered verbatim, so it gets the same neutralisation as the
            // action and the justification. Not trimmed — a diff line's leading
            // spaces are its alignment. Colour is decided from the neutralised
            // text so a leading escape byte cannot borrow the `+` styling.
            let line = neutralize_display_text(raw);
            let style = match line.as_bytes().first() {
                Some(b'+') => added,
                Some(b'-') => removed,
                _ => dim,
            };
            lines.extend(wrap_prefixed("  ", &line, width, style, style));
        }
    }
    lines
}

/// Rows the y/a/n choice block occupies at the bottom of the panel.
const APPROVAL_OPTION_ROWS: u16 = 3;
/// Floor for the whole panel — the historical fixed size.
const APPROVAL_PANEL_MIN_ROWS: u16 = 6;
/// Ceiling for the whole panel. Past this the body scrolls: a long diff
/// preview must not push the transcript off the screen.
const APPROVAL_PANEL_MAX_ROWS: u16 = 16;

/// Everything an [`ApprovalRequest`] contributes to the panel.
///
/// The production path to [`approval_lines`] goes through this struct so that
/// `approval_lines_sanitize_every_text_field` can destructure it exhaustively:
/// adding a field here fails to compile until that test accounts for it. The
/// positional signature alone was not a guard — a new parameter is silenced at
/// every call site by passing `None`, and the sanitisation test stayed green
/// while the field rendered raw. Two real gaps (`preview`, `description`)
/// reached users that way.
///
/// [`ApprovalRequest`]: deep_code_agent::ApprovalRequest
struct ApprovalPanelText<'a> {
    tool_name: &'a str,
    risk: String,
    requires_sandbox: bool,
    network: bool,
    justification: Option<&'a str>,
    resolved_target: Option<&'a str>,
    matched_rule: Option<&'a str>,
    description: &'a str,
    arguments_json: String,
    preview: Option<&'a str>,
    safety_notes: &'a [SafetyNote],
}

impl<'a> ApprovalPanelText<'a> {
    fn from_request(request: &'a deep_code_agent::ApprovalRequest) -> Self {
        Self {
            tool_name: &request.tool_name,
            risk: format!("{:?}", request.risk_level),
            requires_sandbox: request.requires_sandbox,
            network: request.network,
            justification: request.justification.as_deref(),
            resolved_target: request.resolved_target.as_deref(),
            matched_rule: request.matched_rule.as_deref(),
            description: &request.description,
            arguments_json: request.arguments.to_string(),
            preview: request.preview.as_deref(),
            safety_notes: &request.safety_notes,
        }
    }

    /// Rows of [`Self::render`] that form the pinned head. Goes through the
    /// same struct and the same function the body is built from, so the two
    /// cannot disagree about where the head ends.
    fn head_rows(&self, width: usize, lang: Lang) -> usize {
        let action =
            sanitize_panel_text(&extract_action(self.tool_name, &self.arguments_json), 240);
        approval_head_lines(
            self.tool_name,
            &self.risk,
            &action,
            self.resolved_target,
            &self.arguments_json,
            width,
            lang,
        )
        .len()
    }

    fn render(&self, width: usize, lang: Lang) -> Vec<Line<'static>> {
        approval_lines(
            self.tool_name,
            &self.risk,
            self.requires_sandbox,
            self.network,
            self.justification,
            self.resolved_target,
            self.matched_rule,
            self.description,
            &self.arguments_json,
            self.preview,
            self.safety_notes,
            width,
            lang,
        )
    }
}

/// The panel body for the pending approval, wrapped to `width`.
///
/// Shared by the renderer and [`approval_panel_rows`] so the height the layout
/// reserves is measured from the very lines that will be drawn — a panel sized
/// from a second, drifting estimate is how the resolved-target line ends up
/// just off the bottom edge.
fn approval_body(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(request) = app.pending_approval.as_ref() else {
        return Vec::new();
    };
    ApprovalPanelText::from_request(request).render(width, app.lang)
}

/// Rows to reserve for the approval panel, sized to its content.
///
/// A fixed height silently truncated the prompt: three body rows meant a
/// `request_write_root` showed its header, action and boundary warning while
/// the resolved directory — the line the human is told to judge by, and the
/// whole point of resolving before prompting — sat one row below the edge with
/// no overflow indicator. Growing to fit puts every decision-critical line on
/// screen; content past [`APPROVAL_PANEL_MAX_ROWS`] (a long diff preview)
/// still scrolls.
///
/// The old ceiling also capped the panel at half the frame, which read as
/// politeness but was the bug: on a 15-row terminal half a frame cannot hold
/// the prompt, and the rows that fell off the bottom were the resolved target
/// and the overflow indicator both. A share of the screen is not something to
/// negotiate when the alternative is asking the human to approve a directory
/// the panel never named — so the only ceiling left is the frame itself
/// (minus the status row), and [`APPROVAL_PANEL_MAX_ROWS`] on top of it.
fn approval_panel_rows(app: &App, area: ratatui::layout::Rect) -> u16 {
    let width = usize::from(area.width.saturating_sub(2)).max(8);
    let wanted = u16::try_from(approval_body(app, width).len())
        .unwrap_or(u16::MAX)
        .saturating_add(APPROVAL_OPTION_ROWS);
    // Everything below the status row may be taken. `floor` is itself capped
    // by `ceiling`, because `clamp` panics when min > max and a 6-row terminal
    // would otherwise reach that.
    let available = area.height.saturating_sub(1);
    let ceiling = APPROVAL_PANEL_MAX_ROWS.min(available);
    let floor = APPROVAL_PANEL_MIN_ROWS.min(ceiling);
    wanted.clamp(floor, ceiling)
}

fn render_approval_panel(frame: &mut Frame<'_>, app: &mut App, area: ratatui::layout::Rect) {
    if app.pending_approval.is_none() {
        return;
    }
    // Body (scrollable) on top; the y/a/n choices pinned to the bottom rows so
    // they stay visible even when a long command wraps.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(APPROVAL_OPTION_ROWS)])
        .split(area);

    let width = usize::from(chunks[0].width.saturating_sub(2)).max(8);
    let mut body = approval_body(app, width);

    // The head does not scroll, for ANY tool. It is the same lines
    // `approval_body` already produced (drained off the front, so the two
    // cannot drift), lifted out of the scrollable region and drawn above it:
    // the subject of the decision must be on screen at the moment the decision
    // keys are live. `End` — the natural keystroke for reading a long
    // justification — otherwise clamped the body to its bottom and carried that
    // line above the viewport, armed and with no "more above" marker; and a
    // short terminal cut it off below. Pinning covers both, and covering every
    // tool is what lets `approval_armed` below mean the invariant instead of
    // approximating it.
    let pinned_rows = app
        .pending_approval
        .as_ref()
        .map(|request| ApprovalPanelText::from_request(request).head_rows(width, app.lang))
        .unwrap_or(0)
        .min(body.len());
    let pinned: Vec<Line<'static>> = body.drain(..pinned_rows).collect();
    let (pinned_area, chunk_body) = if pinned.is_empty() {
        (None, chunks[0])
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(u16::try_from(pinned.len()).unwrap_or(u16::MAX)),
                Constraint::Min(0),
            ])
            .split(chunks[0]);
        (Some(rows[0]), rows[1])
    };
    // A `Length` longer than the space available is clamped, so this is how a
    // head that did not fit reports itself. Feeds `approval_armed` below.
    let head_drawn = pinned_area.map_or(pinned.is_empty(), |area| {
        usize::from(area.height) >= pinned.len()
    });
    let chunks = [chunk_body, chunks[1]];
    let body_len = body.len();
    // A body taller than its area gives up its last row to an overflow
    // indicator. Without one, a panel that ends mid-content looks like the
    // whole prompt — and "there is more you have not read" is precisely what a
    // boundary prompt must not hide. Reserving the row cannot create the
    // overflow it reports: this branch is only taken when the body already
    // exceeds the untrimmed height.
    let overflows = body_len > usize::from(chunks[0].height);
    let (content_area, hint_area) = if overflows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(chunks[0]);
        (rows[0], Some(rows[1]))
    } else {
        (chunks[0], None)
    };
    // Clamp against the real rendered body (wrapped lines, safety notes, diff
    // preview) so the user can scroll to the very end before deciding. Only the
    // render layer knows the true wrapped height, so it also writes the clamped
    // value back — otherwise PageDown past the end accumulates unbounded and a
    // later PageUp has to burn off the overshoot before the view moves.
    let viewport = usize::from(content_area.height).max(1);
    let max_scroll = body_len.saturating_sub(viewport);
    let scroll = app.approval_scroll_offset.min(max_scroll);
    app.approval_scroll_offset = scroll;
    if let Some(pinned_area) = pinned_area {
        frame.render_widget(
            Paragraph::new(pinned).block(Block::default().padding(Padding::new(1, 0, 0, 0))),
            pinned_area,
        );
    }
    let body_paragraph = Paragraph::new(body)
        .block(Block::default().padding(Padding::new(1, 0, 0, 0)))
        .scroll((scroll as u16, 0));
    frame.render_widget(body_paragraph, content_area);
    if let Some(hint_area) = hint_area
        && max_scroll > scroll
    {
        let hint = tr_with(
            app.lang,
            TextId::ApprovalMoreBelow,
            &[("count", &(max_scroll - scroll).to_string())],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {hint}"),
                Style::default().fg(Color::Yellow),
            )))
            .block(Block::default().padding(Padding::new(1, 0, 0, 0))),
            hint_area,
        );
    }

    let key_y = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let key_a = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key_n = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let focus = app.approval_focus;
    // A root grant offers no "approve for session": consent is per-directory
    // by design, so the option (and its key) disappear rather than silently
    // downgrade.
    let mut options: Vec<(&str, &str, Style)> = vec![
        ("  y", tr(app.lang, TextId::ApprovalOptApprove), key_y),
        ("  a", tr(app.lang, TextId::ApprovalOptSession), key_a),
        ("  n", tr(app.lang, TextId::ApprovalOptDeny), key_n),
    ];
    if app.pending_is_root_grant() {
        options.remove(1);
    }
    let options_body: Vec<Line> = options
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

    let option_count = options_body.len();
    let options =
        Paragraph::new(options_body).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(options, chunks[1]);

    // Armed only now, and only if the rows a decision rests on were actually
    // painted: the whole pinned head — which carries the subject of the
    // decision for every tool — and room for EVERY choice. A viewport showing
    // `y Approve` while `n Deny` fell off the bottom is the worst of the two,
    // since deny is where the focus starts on a root grant.
    //
    // This used to be the first statement in the function, set
    // unconditionally, so a panel squeezed to zero rows — not one cell drawn
    // — still accepted `y`: the queued-keystroke guard was off exactly when
    // the user could see nothing. A frame with no room leaves the prompt
    // disarmed; the next one (a resize, a redraw) arms it.
    //
    // The sole condition used to be `content_area.height > 0` — "at least one
    // body row". That row is the header, which names the tool and never the
    // action, so every non-root-grant prompt armed on a 5- or 6-row terminal
    // with its subject off-screen and (at 5 rows) no overflow marker either.
    // Only root grants were safe, and only because their head was pinned.
    //
    // Pinning the head for EVERY tool is what fixes that, and it does so
    // through this same term: a head too tall for the space takes the whole
    // region, leaving the scrollable remainder zero rows. So `head_drawn` is
    // belt-and-braces today — deliberately, as the invariant stated outright
    // instead of inferred from a `Length` being clamped and a `Min(0)`
    // collapsing, two layouts away. Inference of exactly that kind is what let
    // this panel arm blind twice; a later layout change must not be able to
    // quietly reinstate it.
    app.approval_armed =
        head_drawn && content_area.height > 0 && usize::from(chunks[1].height) >= option_count;
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
    // Borderless composer: just a dim rule above and below, with a "› "
    // prompt marker. The streaming state shows in the status line, so the
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

    // Faint placeholder when the composer is empty and accepting input. While a
    // turn streams the composer is still live (mid-turn steering), so it gets a
    // placeholder too — a different one, since the text will be queued rather
    // than sent immediately. Without this the feature is invisible.
    if app.input.is_empty() && app.pending_approval.is_none() {
        let hint = if app.is_streaming {
            TextId::ComposerPlaceholderSteering
        } else {
            TextId::ComposerPlaceholder
        };
        frame.buffer_mut().set_string(
            text_x,
            inner_area.y,
            tr(app.lang, hint),
            Style::default().fg(Color::DarkGray),
        );
    }

    // The cursor follows the composer whenever it is editable — including mid
    // stream, otherwise steered text would be typed blind. Only the approval
    // prompt takes focus away (keys there are y/a/n, not text).
    if app.pending_approval.is_none() {
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
