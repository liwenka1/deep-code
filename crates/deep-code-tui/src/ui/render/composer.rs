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
pub(super) fn cursor_row_col(input: &str, cursor_chars: usize, width: usize) -> (usize, usize) {
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

pub(super) fn render_input_from_layout(
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
