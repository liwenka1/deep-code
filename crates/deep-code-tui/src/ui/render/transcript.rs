//! Transcript rendering: history cells to styled lines, the drag-selection
//! overlay, and the transcript/clipboard sanitizers.

use super::*;

use crate::app::CachedCellLines;
use crate::history::{ToolApprovalState, ToolBatchEntry, tool_log_label};
use deep_code_agent::ToolResultStatus;

pub(super) fn render_messages(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: ratatui::layout::Rect,
) -> TranscriptSnapshot {
    // No transcript border/title — a 1-col left gutter and the input box
    // below provide all the structure, keeping every column for content.
    let viewport = usize::from(area.height).max(1);
    let content_width = area.width.saturating_sub(2).max(8);

    // Render the WHOLE transcript into a stable line buffer: a fixed
    // coordinate space is what lets mouse drag-selection map cleanly, and
    // bottom-anchored scroll then just windows it.
    //
    // Each cell's lines come from the memo when they are still valid — the
    // transcript is re-rendered every frame, and re-parsing markdown for the
    // whole history measured ~41ms/frame on a 1.7MB session (see
    // `App::cell_lines`). Rendering is per-cell, so the cost is now paid once
    // per cell instead of once per cell per frame.
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Absolute row of every foldable block's header, and what it controls.
    // Recorded here because this is the only place a cell's first rendered row
    // is known; the snapshot turns a click's `(col, row)` back into one of
    // these.
    let mut fold_headers: Vec<(usize, FoldTarget)> = Vec::new();
    for index in 0..app.history.len() {
        let stale = !app
            .cell_lines
            .get(&index)
            .is_some_and(|cached| cached.matches(&app.history[index], content_width, app.lang));
        if stale {
            let cell = app.history[index].clone();
            let rendered = cell_lines(&cell, content_width, app.lang);
            app.cell_lines.insert(
                index,
                CachedCellLines::new(cell, content_width, app.lang, rendered),
            );
        }
        match app.history[index] {
            HistoryCell::Reasoning { .. } => {
                fold_headers.push((lines.len(), FoldTarget::HistoryReasoning(index)));
            }
            HistoryCell::ToolBatch { .. } => {
                fold_headers.push((lines.len(), FoldTarget::HistoryToolBatch(index)));
            }
            _ => {}
        }
        lines.extend(app.cell_lines[&index].lines().iter().cloned());
    }
    // A session switch or `/clear` leaves the memo holding cells that are no
    // longer in `history`. They cost nothing to keep re-validating, but they do
    // hold the old transcript in memory, so drop the ones past the end once the
    // two disagree.
    if app.cell_lines.len() > app.history.len() {
        let live = app.history.len();
        app.cell_lines.retain(|index, _| *index < live);
    }
    let preview = app
        .active_turn
        .as_ref()
        .map(|active| active.preview_cells())
        .unwrap_or_default();
    for cell in &preview {
        // The preview's reasoning block is never in `history` yet, so it gets
        // its own target — toggling it must reach the live turn, not a cell.
        if matches!(cell, HistoryCell::Reasoning { .. }) {
            fold_headers.push((lines.len(), FoldTarget::LiveReasoning));
        }
        lines.extend(cell_lines(cell, content_width, app.lang));
    }

    // Prompts the user steered while this turn streams, drawn under the live
    // preview and above the composer — exactly where each one lands when it is
    // sent. A preview only: these lines never enter `app.history`, so a queued
    // prompt disappears the moment its `UserMessageInjected` retires it.
    // Dimmed and marked, so a promise is never taken for a message already
    // sent.
    if !app.steering_queue.is_empty() {
        lines.push(Line::from(Span::styled(
            tr(app.lang, TextId::PendingSteerLabel),
            Style::default().fg(Color::Yellow),
        )));
        for text in &app.steering_queue {
            lines.extend(pending_steer_lines(text, content_width));
        }
    }

    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = app.scroll_offset.min(max_scroll);
    // Absolute top row, NOT clamped to u16. ratatui's `Paragraph::scroll` offset
    // is a u16, so `scroll_top as u16` WRAPPED (mod 65536) once history crossed
    // 65_535 rendered rows, and clamping to u16::MAX instead pinned the view to
    // row 65_535 — either way the newest output, at the bottom of a taller
    // transcript, became unreachable. Instead of scrolling the Paragraph, window
    // the lines: render from `scroll_top` down with the Paragraph's own offset at
    // 0, so any height is addressable. `plain` and `scroll_top` stay absolute for
    // the selection overlay and the snapshot, so mouse→line mapping is unchanged.
    let scroll_top = max_scroll - scroll;

    let plain: Vec<String> = lines.iter().map(line_plain_text).collect();

    let visible: Vec<Line<'static>> = if scroll_top < lines.len() {
        lines.split_off(scroll_top)
    } else {
        Vec::new()
    };
    let paragraph =
        Paragraph::new(visible).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
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
        fold_headers,
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

/// One queued (steered) prompt: rendered like a user cell but dimmed, with a
/// queue marker instead of the sent `›` — it is a promise, not a message.
fn pending_steer_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = wrap_prefixed(
        "◷ ",
        text,
        width as usize,
        Style::default().fg(Color::DarkGray),
        Style::default().fg(Color::Yellow),
    );
    lines.push(Line::default());
    lines
}

/// The `⏺ name  args` row for one call.
///
/// Built through the standalone cell's own formatter on purpose: one call must
/// not be described two ways depending on whether it happens to sit inside a
/// folded batch. A batch passes `None` for the clock — it only ever holds
/// finished calls.
fn tool_call_lines(
    tool_name: &str,
    arguments: &str,
    approval: ToolApprovalState,
    running_for_secs: Option<u64>,
    lang: Lang,
) -> Vec<Line<'static>> {
    let text = HistoryCell::ToolCall {
        tool_name: tool_name.to_string(),
        arguments: arguments.to_string(),
        approval,
        running_for_secs,
    }
    .lines(lang)
    .join(" ");
    vec![Line::from(vec![
        Span::styled("⏺ ", Style::default().fg(Color::Green)),
        Span::raw(text),
    ])]
}

/// One call and its result, as the transcript shows them.
///
/// Shared by a batch's expanded body and by a run that is still being worked on,
/// so a call's row is the same either way. The entries are quiet successes by
/// construction (see [`crate::history::folded_entry`]), so the call carries no
/// badge and the result is always the successful one.
fn batch_entry_lines(entry: &ToolBatchEntry, width: usize, lang: Lang) -> Vec<Line<'static>> {
    let mut lines = tool_call_lines(
        &entry.tool_name,
        &entry.arguments,
        ToolApprovalState::NotRequired,
        // Flushed calls never carry a clock.
        None,
        lang,
    );
    lines.extend(tool_result_lines(
        &ToolResultStatus::Success,
        &entry.summary,
        width,
    ));
    lines
}

/// The `  ⎿ <word> summary` rows under a call, trailing blank included.
fn tool_result_lines(status: &ToolResultStatus, summary: &str, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let body = match status {
        ToolResultStatus::Success => dim,
        ToolResultStatus::Denied => Style::default().fg(Color::Yellow),
        ToolResultStatus::Error => Style::default().fg(Color::Red),
    };
    let mut lines = wrap_prefixed("  ⎿ ", summary, width, body, dim);
    lines.push(Line::default());
    lines
}

/// The one row a folded run of calls shrinks to — and the row a click lands on
/// to open it.
///
/// Each tool is counted once, in the order the run first met it, so the row
/// reads as the order the work happened rather than as a fixed schema:
/// `read 3 file(s) · 2 command(s) run`. The verbs come from
/// [`tool_log_label`], which is the same table that decides *what may fold at
/// all* — so the two cannot drift, since a tool with no verb can never be in
/// a batch to be labelled.
fn tool_batch_header(entries: &[ToolBatchEntry], expanded: bool, lang: Lang) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for entry in entries {
        match counts.iter_mut().find(|(name, _)| *name == entry.tool_name) {
            Some((_, count)) => *count += 1,
            None => counts.push((entry.tool_name.as_str(), 1)),
        }
    }
    let labels: Vec<String> = counts
        .iter()
        .filter_map(|(name, count)| {
            tool_log_label(name).map(|id| tr_with(lang, id, &[("count", &count.to_string())]))
        })
        .collect();
    let marker = if expanded { "▾" } else { "▸" };
    Line::from(vec![
        Span::styled(format!("{marker} ⏺ "), dim),
        Span::styled(labels.join(" · "), dim),
    ])
}

/// The single row a reasoning block folds down to — and the row a click lands
/// on to unfold it.
///
/// A header is drawn in BOTH states rather than only when folded. Expanded, it
/// is the stable target that folds the block back up; folded, its `▸` marker is
/// the only affordance saying the row can be clicked at all, since this
/// terminal has no hover.
///
/// The line count is the block's own — its newlines, not the rows it would
/// occupy wrapped. Counting display rows would mean wrapping the text purely to
/// measure it, which is the cost folding exists to avoid; a fold that says
/// "42 lines" while hiding rather more of them is the accepted trade. The count
/// costs a scan of the text, which the per-cell memo pays once per cell rather
/// than once per frame.
fn reasoning_header(text: &str, expanded: bool, lang: Lang) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let marker = if expanded { "▾" } else { "▸" };
    let count = text.lines().count().to_string();
    Line::from(vec![
        Span::styled(format!("{marker} ✻ "), dim),
        Span::styled(
            tr_with(lang, TextId::ThinkingHeader, &[("lines", &count)]),
            dim,
        ),
    ])
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
        HistoryCell::Reasoning { text, expanded } => {
            // Folded, the whole block is this one row: `wrap_styled` never runs,
            // so a long reasoning stream no longer re-wraps itself on every
            // frame it arrives in either.
            let mut lines = vec![reasoning_header(text, *expanded, lang)];
            if *expanded {
                lines.extend(wrap_styled(text, width, dim));
            }
            lines.push(Line::default());
            lines
        }
        // Tool call + result form a tight group: a green dot for the call,
        // a dim ⎿ connector for the result. No blank between them.
        HistoryCell::ToolCall {
            tool_name,
            arguments,
            approval,
            running_for_secs,
        } => tool_call_lines(tool_name, arguments, *approval, *running_for_secs, lang),
        HistoryCell::ToolResult {
            status, summary, ..
        } => tool_result_lines(status, summary, width),
        // A folded run: one summary row, or the whole run behind the same row.
        //
        // The row is drawn in BOTH states for the same reason the reasoning
        // header is — expanded, it is the target that folds the run back up,
        // and its marker is the only thing saying the row can be clicked,
        // since this terminal has no hover.
        HistoryCell::ToolBatch { entries, expanded } => {
            let mut lines = vec![tool_batch_header(entries, *expanded, lang)];
            if *expanded {
                for entry in entries {
                    lines.extend(batch_entry_lines(entry, width, lang));
                }
            } else {
                lines.push(Line::default());
            }
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
