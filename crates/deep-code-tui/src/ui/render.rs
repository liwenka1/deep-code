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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(6),
                input_height,
                Constraint::Length(1),
            ])
            .split(frame.area());
        let snap = render_messages(frame, app, chunks[0]);
        render_approval_panel(frame, app, chunks[1]);
        render_input_from_layout(frame, app, &layout, chunks[2]);
        render_status(frame, app, chunks[3]);
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
/// without flicker.
fn cell_lines(cell: &HistoryCell, width: u16, lang: Lang) -> Vec<Line<'static>> {
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
fn extract_action(arguments_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments_json)
        && let Some(object) = value.as_object()
    {
        for key in ["command", "path", "file_path", "url", "pattern", "query"] {
            if let Some(text) = object.get(key).and_then(serde_json::Value::as_str) {
                return crate::history::collapse_whitespace(text);
            }
        }
    }
    crate::history::collapse_whitespace(arguments_json)
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
            tool_name.to_string(),
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

    let action = crate::history::truncate_chars(&extract_action(arguments_json), 240);
    lines.extend(wrap_prefixed(
        "  ",
        &action,
        width,
        Style::default(),
        Style::default(),
    ));

    let description = description.trim();
    if !description.is_empty() && description != action {
        lines.extend(wrap_prefixed("  ", description, width, dim, dim));
    }

    let mut meta = Vec::new();
    // The network ask leads: it is what makes this approval different from an
    // ordinary run of the same command.
    if network {
        meta.push(tr(lang, TextId::ApprovalNetwork).to_string());
    }
    if requires_sandbox {
        meta.push(tr(lang, TextId::ApprovalSandbox).to_string());
    }
    if let Some(rule) = matched_rule {
        meta.push(tr_with(lang, TextId::ApprovalRule, &[("rule", rule)]));
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
            let style = match raw.as_bytes().first() {
                Some(b'+') => added,
                Some(b'-') => removed,
                _ => dim,
            };
            lines.extend(wrap_prefixed("  ", raw, width, style, style));
        }
    }
    lines
}

fn render_approval_panel(frame: &mut Frame<'_>, app: &mut App, area: ratatui::layout::Rect) {
    // Cloned (not borrowed) so the clamped scroll can be written back below; the
    // panel only renders while an approval is pending, so this is rare.
    let Some(request) = app.pending_approval.clone() else {
        return;
    };
    // Body (scrollable) on top; the y/a/n choices pinned to the bottom rows so
    // they stay visible even when a long command wraps.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    let width = usize::from(chunks[0].width.saturating_sub(2)).max(8);
    let body = approval_lines(
        &request.tool_name,
        &format!("{:?}", request.risk_level),
        request.requires_sandbox,
        request.network,
        request.matched_rule.as_deref(),
        &request.description,
        &request.arguments.to_string(),
        request.preview.as_deref(),
        &request.safety_notes,
        width,
        app.lang,
    );
    // Clamp against the real rendered body (wrapped lines, safety notes, diff
    // preview) so the user can scroll to the very end before deciding. Only the
    // render layer knows the true wrapped height, so it also writes the clamped
    // value back — otherwise PageDown past the end accumulates unbounded and a
    // later PageUp has to burn off the overshoot before the view moves.
    let viewport = usize::from(chunks[0].height).max(1);
    let max_scroll = body.len().saturating_sub(viewport);
    let scroll = app.approval_scroll_offset.min(max_scroll);
    app.approval_scroll_offset = scroll;
    let body_paragraph = Paragraph::new(body)
        .block(Block::default().padding(Padding::new(1, 0, 0, 0)))
        .scroll((scroll as u16, 0));
    frame.render_widget(body_paragraph, chunks[0]);

    let key_y = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let key_a = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key_n = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let focus = app.approval_focus;
    let options_body: Vec<Line> = [
        ("  y", tr(app.lang, TextId::ApprovalOptApprove), key_y),
        ("  a", tr(app.lang, TextId::ApprovalOptSession), key_a),
        ("  n", tr(app.lang, TextId::ApprovalOptDeny), key_n),
    ]
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

    let options =
        Paragraph::new(options_body).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(options, chunks[1]);
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

    // Faint placeholder when empty and idle.
    if app.input.is_empty() && !app.is_streaming && app.pending_approval.is_none() {
        frame.buffer_mut().set_string(
            text_x,
            inner_area.y,
            tr(app.lang, TextId::ComposerPlaceholder),
            Style::default().fg(Color::DarkGray),
        );
    }

    if !app.is_streaming && app.pending_approval.is_none() {
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
            extract_action(r#"{"command":"npm run build"}"#),
            "npm run build"
        );
        assert_eq!(
            extract_action(r#"{"path":"src/foo.rs","content":"x"}"#),
            "src/foo.rs"
        );
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
