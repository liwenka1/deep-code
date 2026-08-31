//! Transcript rendering: history cells to styled lines, the drag-selection
//! overlay, and the transcript/clipboard sanitizers.

use super::*;

pub(super) fn render_messages(
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

pub(super) fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Overlay reverse-video on the selected span (post-render buffer styling, so
/// it composes over whatever colours the cells already used).
pub(super) fn highlight_selection(
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
pub(super) fn cell_lines(cell: &HistoryCell, width: u16, lang: Lang) -> Vec<Line<'static>> {
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
pub(super) fn neutralize_transcript_text(text: &str) -> String {
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

pub(super) fn cell_lines_unsanitized(
    cell: &HistoryCell,
    width: u16,
    lang: Lang,
) -> Vec<Line<'static>> {
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
