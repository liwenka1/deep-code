//! The composer input box: its sanitizer, the shared text-layout engine
//! (`layout_input`), and the box renderer itself.

use super::*;

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
    /// The row of the whole input shown at the top of the viewport: where it is
    /// scrolled to, which the next frame passes back in as `scroll_top`.
    pub first_row: usize,
}

/// Layout the text, wrapping at `width`, scrolling to keep the cursor
/// visible, and returning only the `max_visible_rows` subset.
///
/// Guarantee: the row the cursor is on always EXISTS in `visible_lines` (and is
/// counted by `total_rows`). A caret at the right edge of an exactly-filled line
/// belongs to the row that starts there, so this materializes that row. The
/// renderer can then scroll to that row if the terminal gives it a smaller
/// viewport than the one laid out here.
///
/// `scroll_top` is the previous frame's [`LayoutResult::first_row`]. The window
/// stays there while the caret is inside it and moves only as far as it takes to
/// bring the caret back in. Derived from the caret alone, the window pinned the
/// caret to its bottom row, so a click on a row higher up scrolled the text under
/// the pointer the moment the caret moved there. `0` reproduces that derivation
/// exactly.
pub(crate) fn layout_input(
    input: &str,
    cursor_chars: usize,
    width: usize,
    max_visible_rows: usize,
    scroll_top: usize,
) -> LayoutResult {
    layout_composer(
        input,
        Some(cursor_chars),
        width,
        max_visible_rows,
        scroll_top,
    )
}

pub(super) fn layout_recalled_input(
    input: &str,
    width: usize,
    max_visible_rows: usize,
    scroll_top: usize,
) -> LayoutResult {
    layout_composer(input, None, width, max_visible_rows, scroll_top)
}

fn layout_composer(
    input: &str,
    cursor_chars: Option<usize>,
    width: usize,
    max_visible_rows: usize,
    scroll_top: usize,
) -> LayoutResult {
    let mut lines = wrap_input_lines(input, width);
    let max_visible = max_visible_rows.max(1);
    let (cursor_row, cursor_col) = if let Some(cursor_chars) = cursor_chars {
        cursor_row_col(input, cursor_chars, width.max(1))
    } else {
        // The active caret is in the search field. Anchor even a one-row
        // preview to recalled text, including after an exact-width wrap.
        let last_text = lines.iter().rposition(|line| !line.trim().is_empty());
        (last_text.unwrap_or(0), 0)
    };

    // `total_rows` sizes the composer and the renderer needs the caret's row
    // in its viewport, even when that row has no text yet.
    while lines.len() <= cursor_row {
        lines.push(String::new());
    }
    let total_rows = lines.len().max(1);

    // Scroll only as far as it takes to keep the cursor visible.
    let mut start = scroll_top;
    if cursor_row < start {
        start = cursor_row;
    } else if cursor_row >= start + max_visible {
        start = cursor_row + 1 - max_visible;
    }
    // Never so far down that rows the text could fill stand empty: the draft
    // may have shrunk since the last frame.
    start = start.min(lines.len().saturating_sub(max_visible));
    let visible = lines[start..start + lines[start..].len().min(max_visible)].to_vec();
    let cursor_visible_row = cursor_row.saturating_sub(start);

    LayoutResult {
        visible_lines: visible,
        cursor_visible_row,
        cursor_col: cursor_col.min(width.saturating_sub(1)),
        total_rows,
        first_row: start,
    }
}

/// Compute the visual row and column of a character position in a
/// grapheme-cluster-aware way.
///
/// The row this returns for an exactly-filled line was ALREADY right when the
/// boundary bug was reported: a caret at the right edge of a full line does
/// belong on the row that follows it. What was wrong is that no such row existed
/// — the layout never materialized it, so `render_input_from_layout`'s
/// `min(inner_height - 1)` clamp rewrote the caret onto the only row there was,
/// which placed it exactly on the first character of the line the user had just
/// filled. `layout_input` materializes that row now; this function's job is to
/// keep naming the same row it always did.
///
/// The wrap is DEFERRED rather than taken the instant `col` reaches `width`, and
/// that is what keeps the name correct in the one case where taking it eagerly
/// double-counts: a newline at the boundary. `col == width` is a pending state —
/// "the caret sits at the right edge and whatever comes next opens a new row" —
/// consumed when the next grapheme arrives, or after the loop for a caret that
/// ends up there. A `\n` is therefore handled BEFORE that consumption: it moves to
/// the row the caret was already pending on instead of opening a second one.
///
/// That ordering is invisible while the clamp masks everything, and turns into a
/// visible extra blank line the moment the row count becomes honest — which is why
/// it is written down rather than left for the next reader to rediscover. It is
/// pinned by
/// `a_newline_at_an_exactly_filled_line_does_not_add_a_second_row`.
///
/// A row can also end short of `width`: a wide grapheme that does not fit the
/// cell left over starts the next row and leaves that cell blank. A caret just
/// before such a grapheme is drawn on it, at the start of the next row — the
/// same answer the filled row gets, and where a click on the grapheme puts it
/// (`char_index_at`). Drawn in the blank cell instead, it showed up at the far
/// end of the row above the one clicked. Pinned by
/// `a_caret_before_a_glyph_that_did_not_fit_is_drawn_on_it`.
pub(super) fn cursor_row_col(input: &str, cursor_chars: usize, width: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    let mut char_idx = 0usize;

    for grapheme in input.graphemes(true) {
        if char_idx >= cursor_chars {
            // The grapheme after the caret: if it opens the next row for lack
            // of room, the caret goes with it. (A filled row is the pending
            // wrap below; a newline ends the caret's row.)
            if grapheme != "\n" && col != 0 && col < width && col + grapheme.width() > width {
                row += 1;
                col = 0;
            }
            break;
        }
        let num_chars = grapheme.chars().count();
        let next_char_idx = char_idx.saturating_add(num_chars);
        let cursor_inside = cursor_chars < next_char_idx;

        if grapheme == "\n" {
            // Deliberately ahead of the pending-wrap consumption below.
            row += 1;
            col = 0;
            char_idx = next_char_idx;
            if cursor_inside {
                break;
            }
            continue;
        }

        // A line that was filled exactly: the caret is at its right edge, so this
        // grapheme opens the next row.
        if col >= width {
            row += 1;
            col = 0;
        }

        let gw = grapheme.width();
        if col + gw > width && col != 0 {
            row += 1;
            col = 0;
        }
        col += gw;
        if cursor_inside {
            break;
        }
        char_idx = next_char_idx;
    }

    // A caret resting at the right edge of a full line belongs to the row that
    // starts there. `layout_input` materializes that row so the renderer has
    // somewhere to put the caret instead of clamping it back to the first line.
    if col >= width {
        row += 1;
        col = 0;
    }

    (row, col)
}

/// The caret position a click on visual `(row, col)` asks for: the inverse of
/// [`cursor_row_col`], over the rows [`wrap_input_lines`] draws. `row` counts
/// from the top of the whole input, not of the scrolled window.
///
/// A click on a grapheme puts the caret before it — after it on the right half
/// of a wide one (the second cell of `中`). A click right of a row's text puts it
/// at the end of that row, whether the row ended at a newline or left a blank
/// tail because the next grapheme did not fit; one below the last row, at the
/// end of the text. The end of a row that wrapped is the start of the next, and
/// is drawn there.
///
/// The walk is `cursor_row_col`'s, rule for rule and in the same order (a `\n`
/// ahead of a pending wrap), and that is what makes the cell a caret is drawn in
/// map back to a position drawn in that same cell. Pinned by
/// `a_click_on_the_caret_cell_keeps_the_caret_there`, and against the drawn rows
/// by `a_click_on_a_drawn_grapheme_lands_on_it`.
pub(crate) fn char_index_at(input: &str, width: usize, row: usize, col: usize) -> usize {
    let width = width.max(1);
    let mut at_row = 0usize;
    let mut at_col = 0usize;
    let mut char_idx = 0usize;

    for grapheme in input.graphemes(true) {
        let num_chars = grapheme.chars().count();
        if grapheme == "\n" {
            if at_row == row {
                return char_idx;
            }
            at_row += 1;
            at_col = 0;
            char_idx += num_chars;
            continue;
        }
        let gw = grapheme.width();
        // A filled row, or one this grapheme does not fit on: it opens the next.
        if at_col >= width || (at_col + gw > width && at_col != 0) {
            if at_row == row {
                return char_idx;
            }
            at_row += 1;
            at_col = 0;
        }
        if at_row == row && col < at_col + gw {
            let right_half = (col - at_col) * 2 >= gw;
            return if right_half {
                char_idx + num_chars
            } else {
                char_idx
            };
        }
        at_col += gw;
        char_idx += num_chars;
    }
    char_idx
}

/// Split text into logical lines, then wrap each line at `width`.
pub(super) fn wrap_input_lines(input: &str, width: usize) -> Vec<String> {
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

/// The prompt marker on the composer's first row. Continuation rows get a
/// matching indent, so the text starts [`GUTTER`] columns in on every row.
const PROMPT: &str = "› ";
const GUTTER: u16 = 2;

pub(super) fn empty_composer_accepts_hint(app: &App) -> bool {
    app.input.is_empty()
        && app.pending_approval.is_none()
        && !app.history_search_open()
        && !app.model_picker_open()
}

fn default_placeholder(lang: Lang, width: u16) -> Option<&'static str> {
    [
        tr(lang, TextId::ComposerPlaceholder),
        tr(lang, TextId::ComposerPlaceholderShort),
        "/help",
        "/",
    ]
    .into_iter()
    .find(|hint| hint.width() <= usize::from(width))
}

/// Draw the composer and return the text area (including its gutter) and its
/// first absolute visual row for [`composer_snapshot`].
pub(super) fn render_input_from_layout(
    frame: &mut Frame<'_>,
    app: &App,
    layout: &LayoutResult,
    area: Rect,
) -> (Rect, usize) {
    // Draw borders only in rows left over after the draft and its caret row.
    let borders = match usize::from(area.height).saturating_sub(layout.visible_lines.len()) {
        0 => Borders::NONE,
        1 => Borders::TOP,
        _ => Borders::TOP | Borders::BOTTOM,
    };
    let style = Style::default();
    let prompt_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(borders)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner_area = block.inner(area);
    block.render(area, frame.buffer_mut());

    let row_count = layout
        .visible_lines
        .len()
        .min(usize::from(inner_area.height));
    if row_count == 0 {
        return (inner_area, layout.first_row);
    }
    let text_x = inner_area.x.saturating_add(GUTTER);

    // Scroll within the generated rows. Keep the previous top on a click
    // inside the editor so the text stays under the pointer on the next draw.
    let previous_offset = app
        .composer
        .as_ref()
        .filter(|previous| previous.rows > 0 && previous.width == inner_area.width)
        .map_or(0, |previous| {
            previous.first_row.saturating_sub(layout.first_row)
        });
    let cursor_row = layout.cursor_visible_row;
    let first = previous_offset.clamp(
        (cursor_row + 1).saturating_sub(row_count),
        cursor_row.min(layout.visible_lines.len() - row_count),
    );
    let visible = &layout.visible_lines[first..first + row_count];
    let placeholder = if !app.is_streaming && empty_composer_accepts_hint(app) {
        default_placeholder(app.lang, inner_area.width.saturating_sub(GUTTER))
    } else {
        None
    };
    for (row, line_text) in visible.iter().enumerate() {
        let y = inner_area.y.saturating_add(row as u16);
        // "› " on the first row, a matching indent on wrapped continuations.
        if row == 0 {
            frame
                .buffer_mut()
                .set_string(inner_area.x, y, PROMPT, prompt_style);
            if line_text.is_empty()
                && let Some(placeholder) = placeholder
            {
                // Paint the hint where the terminal overlays IME preedit.
                // Layout and click mapping still use the empty draft.
                frame
                    .buffer_mut()
                    .set_string(text_x, y, placeholder, Color::DarkGray);
                continue;
            }
        }
        frame.buffer_mut().set_string(text_x, y, line_text, style);
    }

    // The terminal anchors IME composition to its native cursor. Set that
    // cursor to the same insertion point that keyboard and mouse editing use.
    if app.pending_approval.is_none() && !app.history_search_open() {
        let cursor = ratatui::layout::Position::new(
            text_x.saturating_add(u16::try_from(layout.cursor_col).unwrap_or(u16::MAX)),
            inner_area.y.saturating_add((cursor_row - first) as u16),
        );
        if inner_area.contains(cursor) {
            frame.set_cursor_position(cursor);
        }
    }

    (
        Rect {
            height: row_count as u16,
            ..inner_area
        },
        layout.first_row + first,
    )
}

/// What a click on the composer has to know about the frame just drawn: where
/// its text rows are (`rows`, from [`render_input_from_layout`]), which row of
/// the input is at the top, and the text they were wrapped from.
pub(super) fn composer_snapshot(
    (rows, first_row): (Rect, usize),
    wrap_width: usize,
    text: String,
) -> ComposerSnapshot {
    ComposerSnapshot {
        x: rows.x,
        text_x: rows.x.saturating_add(GUTTER),
        y: rows.y,
        width: rows.width,
        rows: rows.height,
        wrap_width,
        first_row,
        text,
    }
}
