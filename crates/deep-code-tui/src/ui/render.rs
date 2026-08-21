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
    let layout = layout_input(
        &app.input,
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
            let title = session_title(record, lang);
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
    for line in &mut lines {
        for span in &mut line.spans {
            if span.content.chars().any(char::is_control) {
                span.content = neutralize_transcript_text(&span.content).into();
            }
        }
    }
    lines
}

/// Control characters out of a finished transcript span. A tab becomes four
/// spaces rather than one, because code blocks reach here tab-indented and
/// collapsing that to a single column misreads the code; everything else
/// becomes one space, preserving the column count the wrap step already
/// committed to.
fn neutralize_transcript_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("    "),
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
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

/// Control characters become spaces. ratatui carries an escape byte through to
/// the terminal verbatim, so text reaching a panel line with `\x1b` or `\r`
/// intact can erase or overwrite the lines around it — inside the very prompt
/// the human is reading to decide. Zero-width characters need no handling here:
/// ratatui drops them before they reach a cell, so bidi overrides cannot flip
/// a panel line either.
///
/// Kept separate from [`sanitize_panel_text`] for the two callers that must not
/// trim or cap: a diff line's leading spaces carry its alignment, and a tool
/// description is longer than any line cap.
fn neutralize_control_chars(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Model-influenced text about to become an approval panel line: control
/// characters become spaces (see [`neutralize_control_chars`]) and the width is
/// capped. Every free-text argument of [`approval_lines`] passes through one
/// of the two, except `risk`: [`risk_display`] maps it to a `&'static str`, so
/// that one cannot echo its input at all. Pinned by
/// `approval_lines_sanitize_every_text_field`, which asserts one marker per
/// field — and renders the root-grant branch as well, or the fields gated
/// behind it would be passed in and never drawn, pinning nothing.
///
/// The cap is in terminal **columns**, not characters: the panel reserves rows
/// by measuring its own wrapped body, so a cap the layout cannot convert to
/// rows is not a bound at all. Capping 240 *characters* let 240 CJK characters
/// claim 480 columns — seven rows at an 80-column terminal — which is how
/// model-supplied text could still push the resolved grant target past the
/// bottom edge after the height itself had been made content-sized.
fn sanitize_panel_text(text: &str, max_cols: usize) -> String {
    crate::history::truncate_display_width(neutralize_control_chars(text).trim(), max_cols)
}

/// Minimal, borderless approval block matching the welcome/picker style: a
/// risk-coloured `●` + tool, the action it will take (prominent), an optional
/// dim description, and only meaningful metadata (sandbox / matched rule).
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

    let action = sanitize_panel_text(&extract_action(tool_name, arguments_json), 240);
    let is_root_grant = tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL;
    if !is_root_grant {
        lines.extend(wrap_prefixed(
            "  ",
            &action,
            width,
            Style::default(),
            Style::default(),
        ));
    }

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
    if is_root_grant {
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
                &[("path", &action)],
            ),
            width,
            dim,
            dim,
        ));
    }

    // Neutralised but not capped: tool descriptions run past any line cap, and
    // they are written by this crate, not the model — the filter is uniformity
    // with the rest of the panel, not a boundary of its own.
    let description = neutralize_control_chars(description);
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
            let line = neutralize_control_chars(raw);
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
    let body = approval_body(app, width);
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
    // painted: at least one body row, and room for EVERY choice. A viewport
    // showing `y Approve` while `n Deny` fell off the bottom is the worst of
    // the two, since deny is where the focus starts on a root grant.
    //
    // This used to be the first statement in the function, set
    // unconditionally, so a panel squeezed to zero rows — not one cell drawn
    // — still accepted `y`: the queued-keystroke guard was off exactly when
    // the user could see nothing. A frame with no room leaves the prompt
    // disarmed; the next one (a resize, a redraw) arms it.
    app.approval_armed = content_area.height > 0 && usize::from(chunks[1].height) >= option_count;
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
        spans.push(Span::raw(error.clone()));
    } else if let Some(activity) = app.streaming_activity() {
        // While streaming (incl. a long time-to-first-token wait) show an
        // animated indicator so the screen never looks frozen.
        spans.push(Span::styled(activity, Style::default().fg(Color::Cyan)));
        spans.push(Span::styled(
            format!("   {}", tr(app.lang, TextId::StatusEscCancel)),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::raw(app.status_line()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray)),
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
        let lines = cell_lines(&cell, 40, Lang::Zh);
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

    fn welcome_text(offline: bool, lang: Lang) -> String {
        let cell = HistoryCell::Welcome {
            version: "0.1.0".to_string(),
            model: "deepseek-chat".to_string(),
            reasoning: "medium".to_string(),
            offline,
            workspace: "~/code/deep-code".to_string(),
            resumed_turns: None,
            persistent: true,
        };
        cell_lines(&cell, 60, lang)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect()
    }

    #[test]
    fn welcome_cell_shows_model_dir_session_when_online() {
        let text = welcome_text(false, Lang::Zh);
        assert!(text.contains("deep-code") && text.contains("v0.1.0"));
        assert!(text.contains("模型") && text.contains("deepseek-chat"));
        assert!(text.contains("目录") && text.contains("新会话 · 已持久化"));
        assert!(
            !text.contains("/apikey"),
            "online must not nag about apikey"
        );
    }

    #[test]
    fn welcome_cell_prompts_apikey_when_offline() {
        let text = welcome_text(true, Lang::Zh);
        assert!(text.contains("离线模式") && text.contains("/apikey"));
        assert!(
            !text.contains("deepseek-chat"),
            "offline hides the model line"
        );
    }

    #[test]
    fn welcome_cell_renders_english_pack() {
        let text = welcome_text(false, Lang::En);
        assert!(text.contains("Model") && text.contains("deepseek-chat"));
        assert!(text.contains("New session · persisted"));
        assert!(!text.contains("模型"), "no Chinese leaks into en: {text}");
    }

    #[test]
    fn left_truncate_keeps_tail_with_ellipsis() {
        assert_eq!(left_truncate("short", 10), "short");
        assert_eq!(left_truncate("abcdefghij", 5), "…ghij");
    }

    #[test]
    fn extract_action_pulls_command_or_path() {
        assert_eq!(
            extract_action("shell", r#"{"command":"npm run build"}"#),
            "npm run build"
        );
        assert_eq!(
            extract_action("write_file", r#"{"path":"src/foo.rs","content":"x"}"#),
            "src/foo.rs"
        );
    }

    /// A write-root request's action line is its `path` and nothing else. The
    /// generic key scan ranks `command` first, so without the tool-specific
    /// list an extra key would put text of the model's choosing on the action
    /// line of a boundary prompt while the grant landed on `path`.
    #[test]
    fn extract_action_for_a_root_grant_ignores_a_decoy_command_key() {
        let decoy = r#"{"path":"/home/u/.deep-code","command":"cat CHANGELOG.md"}"#;
        assert_eq!(
            extract_action(deep_code_agent::REQUEST_WRITE_ROOT_TOOL, decoy),
            "/home/u/.deep-code"
        );
        // Same payload under any other tool keeps the generic precedence.
        assert_eq!(extract_action("shell", decoy), "cat CHANGELOG.md");
    }

    #[test]
    fn risk_display_maps_tier_to_colour() {
        assert_eq!(risk_display("High", Lang::Zh), ("高风险", Color::Red));
        assert_eq!(risk_display("Medium", Lang::Zh), ("中风险", Color::Yellow));
        assert_eq!(risk_display("Low", Lang::Zh), ("低风险", Color::DarkGray));
        assert_eq!(risk_display("High", Lang::En), ("High risk", Color::Red));
        assert_eq!(risk_display("weird", Lang::Zh).0, "");
    }

    #[test]
    fn approval_lines_are_minimal_no_dump_fields() {
        let lines = approval_lines(
            "shell",
            "Medium",
            false,
            false,
            None,
            None,
            None,
            "运行构建脚本",
            r#"{"command":"npm run build"}"#,
            None,
            &[],
            60,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("需要批准") && text.contains("shell"));
        assert!(text.contains("npm run build") && text.contains("中风险"));
        for noise in ["Risk:", "Sandbox:", "Rule:", "Tool:", "Approval required"] {
            assert!(!text.contains(noise), "must not contain `{noise}`");
        }
        // false/none metadata is hidden.
        assert!(!text.contains("沙箱") && !text.contains("规则"));
    }

    #[test]
    fn approval_lines_render_colored_diff_preview() {
        let preview = "@@ -1,2 +1,2 @@\n one\n-two\n+three";
        let lines = approval_lines(
            "write_file",
            "Medium",
            false,
            false,
            None,
            None,
            None,
            "写入 note.txt",
            r#"{"path":"note.txt"}"#,
            Some(preview),
            &[],
            60,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("变更预览"));
        assert!(text.contains("-two") && text.contains("+three"));

        let style_of = |needle: &str| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains(needle))
                .map(|span| span.style)
                .unwrap_or_else(|| panic!("missing span {needle}"))
        };
        assert_eq!(style_of("+three").fg, Some(Color::Green));
        assert_eq!(style_of("-two").fg, Some(Color::Red));
    }

    /// The model's justification renders as a labelled claim, with control
    /// characters stripped so model text cannot forge extra panel lines or
    /// smuggle escape sequences into a security prompt.
    #[test]
    fn approval_lines_render_justification_as_a_sanitized_claim() {
        let lines = approval_lines(
            "shell",
            "Medium",
            false,
            true,
            Some("need\x1b[31m crates.io\nfor deps"),
            None,
            None,
            "拉取依赖",
            r#"{"command":"cargo fetch"}"#,
            None,
            &[],
            80,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("模型自述理由"), "{text}");
        assert!(
            text.contains("need [31m crates.io for deps"),
            "control chars become spaces: {text}"
        );
        assert!(
            !text.contains('\x1b'),
            "no raw escape bytes reach the panel"
        );
    }

    /// A root-grant approval calls out the boundary change in warning color
    /// and names the resolved directory the grant would actually land on.
    /// When the request's spelling already IS that directory, no symlink
    /// caution appears.
    #[test]
    fn approval_lines_flag_a_root_grant() {
        let lines = approval_lines(
            deep_code_agent::REQUEST_WRITE_ROOT_TOOL,
            "High",
            false,
            false,
            Some("build artifacts live there"),
            Some("/tmp/proj-sibling"),
            None,
            "grants write access",
            r#"{"path":"/tmp/proj-sibling"}"#,
            None,
            &[],
            80,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("/tmp/proj-sibling"), "{text}");
        assert!(text.contains("写权限"), "boundary warning shown: {text}");
        assert!(
            text.contains("实际授予（解析后）"),
            "resolved target labelled: {text}"
        );
        assert!(
            !text.contains("符号链接"),
            "no symlink caution when the spelling matches the target: {text}"
        );
    }

    /// A request whose spelling resolves elsewhere (symlink) must say so and
    /// show the real target — the human judges the resolved directory, not
    /// the model's innocuous-looking spelling.
    #[test]
    fn approval_lines_warn_when_a_root_grant_resolves_elsewhere() {
        let lines = approval_lines(
            deep_code_agent::REQUEST_WRITE_ROOT_TOOL,
            "High",
            false,
            false,
            None,
            Some("/Users/x/secrets"),
            None,
            "grants write access",
            r#"{"path":"/tmp/workspace/build-cache"}"#,
            None,
            &[],
            80,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(
            text.contains("/Users/x/secrets"),
            "the REAL target is shown: {text}"
        );
        assert!(
            text.contains("符号链接"),
            "spelling-vs-target mismatch is called out: {text}"
        );

        // Defensive rendering: a root grant somehow parked without a resolved
        // target must say the prompt cannot vouch for a directory.
        let unresolved = approval_lines(
            deep_code_agent::REQUEST_WRITE_ROOT_TOOL,
            "High",
            false,
            false,
            None,
            None,
            None,
            "grants write access",
            r#"{"path":"/tmp/gone"}"#,
            None,
            &[],
            80,
            Lang::Zh,
        );
        let text: String = unresolved
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("无法解析"), "{text}");
    }

    /// A benign respelling of the same directory — trailing slash, `.`
    /// segments — is NOT accused of resolving elsewhere: the caution must
    /// keep meaning "a link took this somewhere its spelling doesn't say",
    /// or it becomes noise the user learns to skip.
    #[test]
    fn approval_lines_do_not_warn_on_lexical_respellings() {
        for spelling in ["/tmp/proj-sibling/", "/tmp/./proj-sibling"] {
            let lines = approval_lines(
                deep_code_agent::REQUEST_WRITE_ROOT_TOOL,
                "High",
                false,
                false,
                None,
                Some("/tmp/proj-sibling"),
                None,
                "grants write access",
                &format!(r#"{{"path":"{spelling}"}}"#),
                None,
                &[],
                80,
                Lang::Zh,
            );
            let text: String = lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
                .collect();
            assert!(
                !text.contains("符号链接"),
                "{spelling:?} spells the target itself — no caution: {text}"
            );
        }
    }

    /// Every free-text panel line is control-character-sanitized, not just
    /// the justification: a directory name (or command) embedding a newline
    /// or escape byte must not fabricate extra lines in a security prompt.
    /// (The runtime refuses such grant targets outright; this pins the
    /// defense-in-depth layer for anything that still reaches a panel.)
    #[test]
    fn approval_lines_sanitize_the_resolved_target_and_action() {
        let lines = approval_lines(
            deep_code_agent::REQUEST_WRITE_ROOT_TOOL,
            "High",
            false,
            false,
            None,
            Some("/tmp/evil\n[fake panel line]"),
            None,
            "grants write access",
            "{\"path\":\"/tmp/evil\\u001b[2K\"}",
            None,
            &[],
            120,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(
            text.contains("/tmp/evil [fake panel line]"),
            "the newline must render as a space, not a line break: {text}"
        );
        assert!(
            !text.contains('\u{1b}'),
            "escape bytes must not reach the panel: {text}"
        );
    }

    /// Feeds a control-character payload into EVERY free-text field and
    /// asserts field by field that the text survives with the control bytes
    /// gone. Three rules this test exists to enforce, all learned the hard way:
    ///
    /// - One marker per field, asserted individually. A `count() >= n`
    ///   assertion stood here before and hid a live gap: `resolved_target`
    ///   was never rendered at all, and the remaining fields alone satisfied
    ///   the count. Dropping its sanitiser kept the test green.
    /// - Fields gated on `tool_name` need a pass with that tool name. The
    ///   root-grant lines render only for `REQUEST_WRITE_ROOT_TOOL`, so a
    ///   single generic-tool pass pins nothing about them.
    /// - A NEW field must not be able to slip past this test. The positional
    ///   signature was never the guard it looked like: a new parameter is
    ///   silenced at every call site with `None`, and this test — whose
    ///   comment used to claim it covered "every free-text argument" — stayed
    ///   green while the field rendered raw. So the payload is built as an
    ///   [`ApprovalPanelText`] and destructured exhaustively below; adding a
    ///   field to that struct stops compiling until it is handled here.
    #[test]
    fn approval_lines_sanitize_every_text_field() {
        const ESC: &str = "\u{1b}[2K\r";
        let notes = [SafetyNote {
            reason: TextId::SafetyNetworkReason,
            suggestion: TextId::SafetyNetworkSuggestion,
        }];
        // Every field carries a distinct uppercase marker so a missing one
        // names itself. `arguments_json` smuggles its escape as the JSON
        // escape sequence, which serde decodes into a real control byte —
        // the model controls that blob, so that path must be covered too.
        let justification = format!("{ESC}JUSTIFICATION");
        let resolved_target = format!("/tmp/target{ESC}TARGET");
        let matched_rule = format!("builtin:rule{ESC}RULE");
        let description = format!("description{ESC}DESCRIPTION");
        let arguments_json = format!("{{\"command\":\"echo hi{}ACTION\"}}", "\\u001b[2K");
        let preview = format!("+ added{ESC}ADDED\n- removed{ESC}REMOVED\n  context{ESC}CONTEXT");

        let render = |tool: &str| -> String {
            let payload = ApprovalPanelText {
                tool_name: tool,
                risk: format!("High{ESC}RISK"),
                requires_sandbox: true,
                network: true,
                justification: Some(&justification),
                resolved_target: Some(&resolved_target),
                matched_rule: Some(&matched_rule),
                description: &description,
                arguments_json: arguments_json.clone(),
                preview: Some(&preview),
                safety_notes: &notes,
            };
            // Exhaustive destructuring: the compiler rejects this the moment a
            // field is added to the struct, forcing the new field to be given
            // a marker and asserted below rather than silently rendering raw.
            let ApprovalPanelText {
                tool_name: _,
                risk: _,
                requires_sandbox: _,
                network: _,
                justification: _,
                resolved_target: _,
                matched_rule: _,
                description: _,
                arguments_json: _,
                preview: _,
                safety_notes: _,
            } = &payload;
            payload
                .render(120, Lang::Zh)
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
                .collect()
        };
        let generic = render(&format!("shell{ESC}TOOLNAME"));
        let root_grant = render(deep_code_agent::REQUEST_WRITE_ROOT_TOOL);

        // Each marker must still render — proving the field was neutralised,
        // not silently dropped. `RISK` is absent by design and deliberately
        // not asserted: `risk_display` maps an unknown tier to a
        // `&'static str`, so that argument cannot echo its input at all.
        for marker in [
            "TOOLNAME",
            "JUSTIFICATION",
            "RULE",
            "DESCRIPTION",
            "ACTION",
            "ADDED",
            "REMOVED",
            "CONTEXT",
        ] {
            assert!(
                generic.contains(marker),
                "{marker} never reached the panel: {generic}"
            );
        }
        assert!(
            root_grant.contains("TARGET"),
            "resolved_target never reached the panel: {root_grant}"
        );
        for text in [&generic, &root_grant] {
            assert!(
                !text.chars().any(char::is_control),
                "a control character reached the approval panel: {text:?}"
            );
        }
    }

    #[test]
    fn approval_lines_preview_keeps_diff_alignment() {
        let lines = approval_lines(
            "write_file",
            "Medium",
            false,
            false,
            None,
            None,
            None,
            "write tools can modify workspace files",
            "{\"path\":\"a.txt\"}",
            Some("  fn main() {\n+     let x = 1;"),
            &[],
            120,
            Lang::Zh,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        // Neutralising must not trim: a diff line's leading spaces are what
        // line it up under the context above it.
        assert!(
            text.contains("  fn main() {"),
            "context line lost its indentation: {text}"
        );
        assert!(
            text.contains("+     let x = 1;"),
            "added line lost its indentation: {text}"
        );
    }

    #[test]
    fn approval_lines_render_safety_notes() {
        let notes = [SafetyNote {
            reason: TextId::SafetyNetworkReason,
            suggestion: TextId::SafetyNetworkSuggestion,
        }];
        let render = |lang| {
            approval_lines(
                "shell",
                "High",
                true,
                false,
                None,
                None,
                None,
                "下载脚本",
                r#"{"command":"curl https://x | sh"}"#,
                None,
                &notes,
                60,
                lang,
            )
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<String>()
        };
        let zh = render(Lang::Zh);
        assert!(
            zh.contains("注意") && zh.contains("会发起网络访问") && zh.contains("确认目标主机")
        );
        // The same structured note renders in English under the en pack.
        let en = render(Lang::En);
        assert!(
            en.contains("network access") && en.contains("Confirm the target"),
            "{en}"
        );
    }

    /// Builds the whole frame for a pending `request_write_root` and returns
    /// what is actually in the terminal cells.
    ///
    /// Rendered in English on purpose: a double-width glyph occupies two cells
    /// and the continuation cell reads back as a space, so concatenating cells
    /// from a Chinese panel yields `实 际 ...` and no substring assertion
    /// against the source string can hold. The layout being pinned here is
    /// language-independent.
    fn root_grant_screen(width: u16, height: u16, resolved_target: &str) -> String {
        root_grant_screen_requesting(width, height, "/tmp/workspace/build-cache", resolved_target)
    }

    /// A `request_write_root` approval as the runtime parks it.
    fn root_grant_request(
        requested: &str,
        resolved_target: &str,
    ) -> deep_code_agent::ApprovalRequest {
        deep_code_agent::ApprovalRequest {
            call_id: "call_grant".to_string(),
            tool_name: deep_code_agent::REQUEST_WRITE_ROOT_TOOL.to_string(),
            description: "grants write access to a directory outside the current roots".to_string(),
            arguments: serde_json::json!({
                "path": requested,
                "justification": "the build writes its artifacts there",
            }),
            risk_level: deep_code_agent::RiskLevel::High,
            requires_sandbox: false,
            network: false,
            justification: Some("the build writes its artifacts there".to_string()),
            resolved_target: Some(resolved_target.to_string()),
            read_only: false,
            matched_rule: Some("builtin:root_grant".to_string()),
            preview: None,
            safety_notes: Vec::new(),
        }
    }

    /// As [`root_grant_screen`], but the model's requested spelling is the
    /// caller's — the field it fully controls, in both length and glyph width.
    fn root_grant_screen_requesting(
        width: u16,
        height: u16,
        requested: &str,
        resolved_target: &str,
    ) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.lang = Lang::En;
        app.pending_approval = Some(root_grant_request(requested, resolved_target));

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for row in 0..buffer.area.height {
            let mut line = String::new();
            for col in 0..buffer.area.width {
                line.push_str(buffer[(col, row)].symbol());
            }
            rows.push(line);
        }
        rows.join("\n")
    }

    /// The resolved directory has to be ON SCREEN — not merely present in the
    /// body vector that `approval_lines` returns.
    ///
    /// Every other approval test inspects that vector, which is why a panel
    /// pinned to six rows (three of them the y/n block) went unnoticed: the
    /// header, action and boundary warning filled the viewport and the
    /// resolved target — the one line the panel tells the human to judge by,
    /// and the entire reason the runtime resolves before prompting — sat below
    /// the bottom edge with no overflow indicator. Asserted at several sizes
    /// because the old height was a constant, so it failed identically on a
    /// large terminal.
    /// Model text in the TRANSCRIPT must not be able to reach the terminal
    /// with control bytes intact, because the approval panel is drawn into the
    /// same frame and a single escape defeats every sanitizer the panel has.
    ///
    /// `\x1b[8m` is SGR conceal: ratatui emits `NoHidden` only when its own
    /// tracked modifier had HIDDEN, so it never turns the attribute back off
    /// and every cell flushed after it — the entire prompt below — renders
    /// invisible. `\x1b[12;3H` repositions the cursor and paints text at a
    /// chosen row, which is how a counterfeit resolved-target line appears in
    /// a prompt that never rendered one. `\r` overwrites the line in place.
    #[test]
    fn transcript_text_cannot_carry_an_escape_into_a_cell() {
        use crate::history::HistoryCell;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let payloads = [
            "\u{1b}[8mconceal everything after me",
            "\u{1b}[12;3HGrant target (resolved): /tmp/harmless",
            "overwrite\rme",
            "bell\u{7}and\u{9b}csi",
        ];
        for payload in payloads {
            for cell in [
                HistoryCell::Assistant {
                    text: payload.to_string(),
                },
                HistoryCell::User {
                    text: payload.to_string(),
                },
                HistoryCell::System {
                    text: payload.to_string(),
                },
            ] {
                let mut app = App::new();
                app.lang = Lang::En;
                app.history.push(cell);
                let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();

                let buffer = terminal.backend().buffer().clone();
                for row in 0..buffer.area.height {
                    for col in 0..buffer.area.width {
                        let symbol = buffer[(col, row)].symbol();
                        assert!(
                            !symbol.chars().any(char::is_control),
                            "control char {:?} reached cell ({col},{row}) from {payload:?}",
                            symbol
                        );
                    }
                }
            }
        }
    }

    /// A small terminal is the third form of the same bug, and the one every
    /// other test in this file was blind to: they all use heights of 20 or
    /// more. A tmux split, a VS Code panel or a short window put the frame at
    /// 12-15 rows, and there the constraint solver handed the transcript its
    /// `Min(5)` and let the panel absorb the whole deficit — at 11 and 12 rows
    /// the `Deny` choice was on screen and pressable while the directory being
    /// granted was not, with no overflow indicator either.
    ///
    /// Swept row by row rather than at a couple of sizes, because the failure
    /// was a boundary: it appeared at exactly the heights nobody sampled.
    #[test]
    fn root_grant_panel_shows_the_resolved_target_on_every_usable_height() {
        let target = "/home/u/.ssh";
        let mut blind = Vec::new();
        for height in 8..=24u16 {
            let screen = root_grant_screen(80, height, target);
            // Where a decision can be made, the decision's subject must be
            // legible. (Below that the panel simply has no room, and the
            // arming guard keeps the keys inert — pinned separately.)
            if screen.contains(tr(Lang::En, TextId::ApprovalOptDeny)) && !screen.contains(target) {
                blind.push(height);
            }
        }
        assert!(
            blind.is_empty(),
            "at heights {blind:?} the user can press Deny/Approve without ever \
             being shown the directory being granted"
        );
    }

    /// Nothing painted must mean nothing decidable. `approval_armed` used to
    /// be set as the first statement of `render_approval_panel`, before and
    /// regardless of any drawing, so a frame with no room for the panel still
    /// accepted `y` — the queued-keystroke guard was disabled precisely when
    /// the user could see nothing at all.
    #[test]
    fn a_panel_with_no_room_to_draw_does_not_arm_the_decision_keys() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for (width, height) in [(20, 1), (20, 2), (40, 3)] {
            let mut app = App::new();
            app.lang = Lang::En;
            app.pending_approval = Some(root_grant_request("/tmp/x", "/home/u/.ssh"));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            let buffer = terminal.backend().buffer().clone();
            let painted: String = (0..buffer.area.height)
                .flat_map(|row| (0..buffer.area.width).map(move |col| (col, row)))
                .map(|(col, row)| buffer[(col, row)].symbol().to_string())
                .collect();
            let drew_choices = painted.contains(tr(Lang::En, TextId::ApprovalOptDeny));
            assert_eq!(
                app.approval_armed, drew_choices,
                "at {width}x{height} the panel armed={} while drew_choices={drew_choices}",
                app.approval_armed
            );
        }
    }

    #[test]
    fn root_grant_panel_shows_the_resolved_target_on_screen() {
        let target = "/home/u/.config/private-keys";
        for (width, height) in [(80, 24), (100, 40), (200, 60)] {
            let screen = root_grant_screen(width, height, target);
            assert!(
                screen.contains(target),
                "the resolved target must be visible at {width}x{height}:\n{screen}"
            );
            // The boundary warning and the y/n choices share the panel with
            // it — none of the three may push another off the edge.
            assert!(
                screen.contains(tr(Lang::En, TextId::ApprovalRootGrant)),
                "the boundary warning must stay visible at {width}x{height}:\n{screen}"
            );
            assert!(
                screen.contains(tr(Lang::En, TextId::ApprovalOptDeny)),
                "the choices must stay visible at {width}x{height}:\n{screen}"
            );
        }
    }

    /// The model controls the *width* of its requested spelling, so the panel
    /// must not let that spelling decide whether the resolved target is on
    /// screen.
    ///
    /// This is the second form of the bug a content-sized panel was meant to
    /// end. Sizing to content fixed the fixed-height version; it did not stop
    /// the spelling — rendered first, and capped at 240 *characters* — from
    /// claiming 480 columns of CJK, seven rows of an 80-column terminal, and
    /// pushing the resolved directory back off the bottom. Whatever the
    /// spelling costs, the target, the boundary warning and the choices stay
    /// visible, and anything below the fold is announced.
    #[test]
    fn a_wide_requested_spelling_cannot_push_the_resolved_target_off_screen() {
        let target = "/home/u/.config/private-keys";
        // 240 characters — the old cap — of a double-width glyph.
        let requested = format!("/{}", "构".repeat(240));
        for (width, height) in [(80, 24), (60, 20), (100, 40)] {
            let screen = root_grant_screen_requesting(width, height, &requested, target);
            assert!(
                screen.contains(target),
                "the resolved target must survive a wide spelling at {width}x{height}:\n{screen}"
            );
            // A fragment, not the whole sentence: at 60 columns the warning
            // legitimately wraps, and a wrapped line is not a missing one.
            assert!(
                screen.contains("Grants WRITE access"),
                "the boundary warning must survive it at {width}x{height}:\n{screen}"
            );
            assert!(
                screen.contains(tr(Lang::En, TextId::ApprovalOptDeny)),
                "the choices must survive it at {width}x{height}:\n{screen}"
            );
        }
    }

    /// The resolved directory is rendered BEFORE the model's spelling, and the
    /// spelling is labelled as the untrusted request.
    ///
    /// Order is a security property here, not typography. An unlabelled action
    /// line wraps into continuation rows carrying the same two-space indent as
    /// a field row, so a spelling ending in `.../Grant target (resolved):
    /// /tmp/safe` paints a counterfeit target line. Drawing the real one first
    /// means the counterfeit can only ever appear *below* the truth, under a
    /// line that says the text is what the model asked for.
    #[test]
    fn the_resolved_target_precedes_the_requested_spelling_it_could_counterfeit() {
        let target = "/home/u/.ssh";
        let counterfeit = "Grant target (resolved): /tmp/harmless";
        let requested = format!("/tmp/pad/{counterfeit}");
        let screen = root_grant_screen_requesting(200, 40, &requested, target);

        let real_row = screen
            .lines()
            .position(|line| line.contains(target))
            .expect("the resolved target must be on screen");
        let label_row = screen
            .lines()
            .position(|line| line.contains("Requested spelling (untrusted)"))
            .expect("the requested spelling must be labelled as untrusted");
        assert!(
            real_row < label_row,
            "the resolved target must be drawn above the spelling it could be confused with:\n{screen}"
        );
        // And the counterfeit, when it appears, is under that label.
        let counterfeit_row = screen
            .lines()
            .position(|line| line.contains("/tmp/harmless"))
            .expect("the spelling itself is still shown");
        assert!(
            label_row <= counterfeit_row,
            "a counterfeit target line must fall under the untrusted label:\n{screen}"
        );
    }

    /// A body taller than the panel says so. Silence reads as "this is the
    /// whole prompt", which is the wrong thing for a boundary decision.
    #[test]
    fn an_overflowing_approval_panel_announces_what_is_below_the_fold() {
        // A long preview guarantees more body lines than any panel height.
        let mut app = App::new();
        app.lang = Lang::En;
        app.pending_approval = Some(deep_code_agent::ApprovalRequest {
            call_id: "call_big".to_string(),
            tool_name: "write_file".to_string(),
            description: "writes a file".to_string(),
            arguments: serde_json::json!({"path": "/tmp/x", "content": "y"}),
            risk_level: deep_code_agent::RiskLevel::Medium,
            requires_sandbox: false,
            network: false,
            justification: None,
            resolved_target: None,
            read_only: false,
            matched_rule: None,
            preview: Some(
                (0..60)
                    .map(|i| format!("+ line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            safety_notes: Vec::new(),
        });

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for row in 0..buffer.area.height {
            for col in 0..buffer.area.width {
                screen.push_str(buffer[(col, row)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("more line(s)"),
            "an overflowing panel must say so:\n{screen}"
        );
        assert!(
            screen.contains(tr(Lang::En, TextId::ApprovalOptDeny)),
            "the choices stay pinned even when the body overflows:\n{screen}"
        );
    }

    /// A root grant's action line shows its `path`; a decoy key cannot occupy
    /// the line the human reads, all the way through to the rendered cells.
    #[test]
    fn root_grant_panel_never_shows_a_decoy_command_on_the_action_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.lang = Lang::En;
        app.pending_approval = Some(deep_code_agent::ApprovalRequest {
            call_id: "call_grant".to_string(),
            tool_name: deep_code_agent::REQUEST_WRITE_ROOT_TOOL.to_string(),
            description: "grants write access".to_string(),
            arguments: serde_json::json!({
                "path": "/home/u/.deep-code",
                "command": "cat CHANGELOG.md",
            }),
            risk_level: deep_code_agent::RiskLevel::High,
            requires_sandbox: false,
            network: false,
            justification: None,
            resolved_target: Some("/home/u/.deep-code".to_string()),
            read_only: false,
            matched_rule: None,
            preview: None,
            safety_notes: Vec::new(),
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for row in 0..buffer.area.height {
            for col in 0..buffer.area.width {
                screen.push_str(buffer[(col, row)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            !screen.contains("cat CHANGELOG.md"),
            "a decoy key must never reach the panel:\n{screen}"
        );
        assert!(
            screen.contains("/home/u/.deep-code"),
            "the real subject must be shown:\n{screen}"
        );
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
                .draw(|frame| render_completion_menu(frame, &menu, frame.area(), Lang::Zh))
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
        let lines = cell_lines(&cell, 20, Lang::Zh);
        assert!(lines.len() >= 4);
        for line in &lines {
            assert!(line_width(line) <= 20);
        }
    }
}
