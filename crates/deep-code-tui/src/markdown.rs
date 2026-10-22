//! Markdown-lite rendering for assistant transcript cells.
//!
//! Two phases, following the reference design: a width-independent block
//! parse ([`parse_blocks`]) and a width-dependent line layout
//! ([`render_markdown`]). Resizing only re-runs the second phase. Fenced
//! code, headings, bullet and numbered lists, pipe tables, and the inline
//! forms a model actually writes — `code`, `**bold**`, `*italic*`, `[links](url)` —
//! are styled; everything else stays plain text.

use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Column alignment carried by a table separator row (`:--`, `:-:`, `--:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Heading {
        level: u8,
        text: String,
    },
    CodeBlock {
        language: Option<String>,
        lines: Vec<String>,
    },
    ListItem {
        indent: usize,
        /// The marker as written: `None` for a bullet, `Some("3.")` for a
        /// numbered item. Kept rather than normalized so a list that starts
        /// at 2, or restarts, renders as the author numbered it — and so a
        /// numbered list is not silently turned into bullets, which lost the
        /// ordinal every time a model wrote "step 1 … step 7".
        number: Option<String>,
        text: String,
    },
    Table {
        alignments: Vec<ColumnAlign>,
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Paragraph {
        text: String,
    },
    Blank,
}

#[must_use]
pub fn parse_blocks(text: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix("```") {
            let language = rest.trim();
            let language = (!language.is_empty()).then(|| language.to_string());
            let mut code = Vec::new();
            index += 1;
            while index < lines.len() && !lines[index].trim_start().starts_with("```") {
                code.push(lines[index].to_string());
                index += 1;
            }
            // Step over the closing fence; when it never arrived (common
            // while a reply was cancelled mid-stream) the collected lines
            // still render as a code block instead of being dropped.
            index += 1;
            blocks.push(MarkdownBlock::CodeBlock {
                language,
                lines: code,
            });
            continue;
        }
        if trimmed.is_empty() {
            blocks.push(MarkdownBlock::Blank);
            index += 1;
            continue;
        }
        if let Some((level, rest)) = parse_heading(trimmed) {
            blocks.push(MarkdownBlock::Heading {
                level,
                text: rest.to_string(),
            });
            index += 1;
            continue;
        }
        if let Some((number, rest)) = parse_list_item(trimmed) {
            blocks.push(MarkdownBlock::ListItem {
                indent: line.len() - trimmed.len(),
                number: number.map(str::to_string),
                text: rest.to_string(),
            });
            index += 1;
            continue;
        }
        if let Some((alignments, header)) =
            parse_table_header(trimmed, lines.get(index + 1).copied())
        {
            let columns = header.len();
            let mut rows = Vec::new();
            index += 2;
            while index < lines.len() && is_table_row(lines[index]) {
                // GFM: short rows pad with empty cells, long rows drop extras.
                let mut cells = split_cells(lines[index]);
                cells.truncate(columns);
                cells.resize(columns, String::new());
                rows.push(cells);
                index += 1;
            }
            blocks.push(MarkdownBlock::Table {
                alignments,
                header,
                rows,
            });
            continue;
        }
        blocks.push(MarkdownBlock::Paragraph {
            text: line.to_string(),
        });
        index += 1;
    }
    blocks
}

#[must_use]
pub fn render_markdown(text: &str, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width).max(8);
    let mut out = Vec::new();
    for block in parse_blocks(text) {
        match block {
            MarkdownBlock::Blank => out.push(Line::default()),
            MarkdownBlock::Heading { level, text } => {
                let prefix = "#".repeat(usize::from(level));
                out.extend(wrap_spans(
                    vec![(
                        format!("{prefix} {text}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    )],
                    width,
                    0,
                ));
            }
            MarkdownBlock::CodeBlock { language, lines } => {
                let marker = match &language {
                    Some(language) => format!("``` {language}"),
                    None => "```".to_string(),
                };
                out.push(Line::styled(marker, fence_style()));
                // Unknown language tokens (or no token) keep the plain
                // single-color style rather than guessing a grammar.
                let highlighted = language
                    .as_deref()
                    .and_then(|token| crate::highlight::highlight_block(token, &lines));
                match highlighted {
                    Some(rows) => {
                        for row in rows {
                            let mut spans = vec![("  ".to_string(), Style::default())];
                            spans.extend(row);
                            out.extend(wrap_spans(spans, width, 2));
                        }
                    }
                    None => {
                        for code in lines {
                            out.extend(wrap_spans(
                                vec![(format!("  {code}"), code_style())],
                                width,
                                2,
                            ));
                        }
                    }
                }
                out.push(Line::styled("```", fence_style()));
            }
            MarkdownBlock::ListItem {
                indent,
                number,
                text,
            } => {
                let pad = " ".repeat(indent.min(8));
                let marker = match &number {
                    Some(number) => format!("{number} "),
                    None => "• ".to_string(),
                };
                let hanging = pad.len() + marker.width();
                let mut spans = vec![(format!("{pad}{marker}"), Style::default())];
                spans.extend(inline_spans(&text));
                out.extend(wrap_spans(spans, width, hanging));
            }
            MarkdownBlock::Table {
                alignments,
                header,
                rows,
            } => {
                out.extend(render_table(&alignments, &header, &rows, width));
            }
            MarkdownBlock::Paragraph { text } => {
                out.extend(wrap_spans(inline_spans(&text), width, 0));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&level) {
        line[level..]
            .strip_prefix(' ')
            .map(|rest| (level as u8, rest))
    } else {
        None
    }
}

/// A list item's marker (`None` for a bullet, the `3.` text for a numbered
/// one) and its text.
fn parse_list_item(line: &str) -> Option<(Option<&str>, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some((None, rest));
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        line[digits..]
            .strip_prefix(". ")
            .map(|rest| (Some(&line[..digits + 1]), rest))
    } else {
        None
    }
}

/// A pipe line becomes a table header only once the separator row below it
/// has arrived — a half-streamed header stays a plain paragraph instead of
/// flickering between shapes (the code-fence rule from the other direction).
fn parse_table_header(line: &str, next: Option<&str>) -> Option<(Vec<ColumnAlign>, Vec<String>)> {
    if !line.contains('|') {
        return None;
    }
    let header = split_cells(line);
    let alignments = parse_separator_row(next?, header.len())?;
    Some((alignments, header))
}

/// A separator row (`| --- | :-: |`) confirms the line above as a header and
/// carries per-column alignment. The column count must match the header's.
fn parse_separator_row(line: &str, columns: usize) -> Option<Vec<ColumnAlign>> {
    let trimmed = line.trim();
    if columns == 0 || !trimmed.contains('|') || !trimmed.contains('-') {
        return None;
    }
    let cells = split_cells(trimmed);
    if cells.len() != columns {
        return None;
    }
    cells
        .iter()
        .map(|cell| parse_separator_cell(cell))
        .collect()
}

fn parse_separator_cell(cell: &str) -> Option<ColumnAlign> {
    let left = cell.starts_with(':');
    let dashes = cell.strip_prefix(':').unwrap_or(cell);
    let (dashes, right) = match dashes.strip_suffix(':') {
        Some(rest) => (rest, true),
        None => (dashes, false),
    };
    if dashes.is_empty() || !dashes.bytes().all(|byte| byte == b'-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => ColumnAlign::Center,
        (false, true) => ColumnAlign::Right,
        _ => ColumnAlign::Left,
    })
}

fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.contains('|') && !trimmed.starts_with("```")
}

/// Split a table row into cells on unescaped `|`; `\|` stays a literal pipe.
/// One leading/trailing empty cell from outer pipes is dropped.
fn split_cells(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            cell.push('|');
            chars.next();
        } else if ch == '|' {
            cells.push(cell.trim().to_string());
            cell = String::new();
        } else {
            cell.push(ch);
        }
    }
    cells.push(cell.trim().to_string());
    if cells.len() > 1 && cells.first().is_some_and(|cell| cell.is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|cell| cell.is_empty()) {
        cells.pop();
    }
    cells
}

/// Gap drawn between table columns; the rule row uses the matching junction.
const COLUMN_GAP: &str = " │ ";

/// Lay a pipe table out to `width`. Columns take their natural width when
/// everything fits; otherwise the widest column shrinks first and cell text
/// wraps inside its column. When even minimum widths cannot fit, fall back
/// to the raw `a | b` text — degraded but never dropped.
fn render_table(
    alignments: &[ColumnAlign],
    header: &[String],
    rows: &[Vec<String>],
    width: usize,
) -> Vec<Line<'static>> {
    const MIN_COLUMN: usize = 3;
    let columns = alignments.len();
    let chrome = COLUMN_GAP.width() * columns.saturating_sub(1);
    let available = width.saturating_sub(chrome);
    if available < columns * MIN_COLUMN {
        return table_fallback_lines(header, rows, width);
    }

    let header_spans: Vec<Vec<(String, Style)>> = header
        .iter()
        .map(|cell| bold_spans(inline_spans(cell)))
        .collect();
    let row_spans: Vec<Vec<Vec<(String, Style)>>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| inline_spans(cell)).collect())
        .collect();

    let mut widths = vec![1usize; columns];
    for (column, spans) in header_spans.iter().enumerate() {
        widths[column] = widths[column].max(spans_width(spans)).min(available);
    }
    for row in &row_spans {
        for (column, spans) in row.iter().enumerate() {
            widths[column] = widths[column].max(spans_width(spans)).min(available);
        }
    }
    while widths.iter().sum::<usize>() > available {
        let Some(widest) = (0..columns).max_by_key(|&column| widths[column]) else {
            break;
        };
        if widths[widest] <= MIN_COLUMN {
            break;
        }
        widths[widest] -= 1;
    }

    let mut out = Vec::new();
    out.extend(table_row_lines(&header_spans, &widths, alignments));
    out.push(table_rule_line(&widths));
    for row in &row_spans {
        out.extend(table_row_lines(row, &widths, alignments));
    }
    out
}

/// One logical table row as rendered lines: every cell wraps inside its
/// column width, shorter cells pad with blanks so the row stays rectangular.
fn table_row_lines(
    cells: &[Vec<(String, Style)>],
    widths: &[usize],
    alignments: &[ColumnAlign],
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
        .iter()
        .zip(widths)
        .map(|(spans, cell_width)| wrap_spans_raw(spans.clone(), *cell_width, 0))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    let mut lines = Vec::new();
    for row_line in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (column, cell) in wrapped.iter().enumerate() {
            if column > 0 {
                spans.push(Span::styled(COLUMN_GAP, table_chrome_style()));
            }
            let content: &[Span<'static>] = cell.get(row_line).map_or(&[], Vec::as_slice);
            let used: usize = content
                .iter()
                .map(|span| span.content.as_ref().width())
                .sum();
            let pad = widths[column].saturating_sub(used);
            let align = alignments.get(column).copied().unwrap_or(ColumnAlign::Left);
            let (before, mut after) = match align {
                ColumnAlign::Left => (0, pad),
                ColumnAlign::Right => (pad, 0),
                ColumnAlign::Center => (pad / 2, pad - pad / 2),
            };
            // Trailing pad on the last column is invisible — skip it so
            // selected/copied lines don't carry a wall of spaces.
            if column + 1 == wrapped.len() {
                after = 0;
            }
            if before > 0 {
                spans.push(Span::raw(" ".repeat(before)));
            }
            spans.extend(content.iter().cloned());
            if after > 0 {
                spans.push(Span::raw(" ".repeat(after)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn table_rule_line(widths: &[usize]) -> Line<'static> {
    let mut rule = String::new();
    for (column, cell_width) in widths.iter().enumerate() {
        if column > 0 {
            rule.push_str("─┼─");
        }
        rule.push_str(&"─".repeat(*cell_width));
    }
    Line::styled(rule, table_chrome_style())
}

/// Plain-text degradation for tables the width cannot hold.
fn table_fallback_lines(
    header: &[String],
    rows: &[Vec<String>],
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = wrap_spans(inline_spans(&header.join(" | ")), width, 0);
    for row in rows {
        lines.extend(wrap_spans(inline_spans(&row.join(" | ")), width, 0));
    }
    lines
}

fn bold_spans(spans: Vec<(String, Style)>) -> Vec<(String, Style)> {
    spans
        .into_iter()
        .map(|(text, style)| (text, style.add_modifier(Modifier::BOLD)))
        .collect()
}

fn spans_width(spans: &[(String, Style)]) -> usize {
    spans.iter().map(|(text, _)| text.as_str().width()).sum()
}

/// Split a paragraph into styled spans: `code`, `**bold**`, `*italic*` /
/// `_italic_`, and `[text](url)` links. Code wins over everything inside it;
/// bold and italic nest in each other. A marker with no closer, or an
/// emphasis whose content starts or ends with a space (`a * b`), stays
/// literal — the model's prose is not rewritten on a guess.
fn inline_spans(text: &str) -> Vec<(String, Style)> {
    let mut spans = inline_styled(text, Style::default());
    if spans.is_empty() {
        spans.push((String::new(), Style::default()));
    }
    spans
}

fn inline_styled(text: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;
    let flush = |plain: &mut String, spans: &mut Vec<(String, Style)>| {
        if !plain.is_empty() {
            spans.push((std::mem::take(plain), base));
        }
    };
    while index < chars.len() {
        let ch = chars[index];
        if ch == '`'
            && let Some(close) = find_char(&chars, index + 1, '`')
        {
            flush(&mut plain, &mut spans);
            let code: String = chars[index + 1..close].iter().collect();
            spans.push((code, base.patch(code_style())));
            index = close + 1;
            continue;
        }
        if ch == '*'
            && chars.get(index + 1) == Some(&'*')
            && let Some(close) = find_pair(&chars, index + 2, '*')
            && emphasis_content_ok(&chars[index + 2..close])
        {
            flush(&mut plain, &mut spans);
            let inner: String = chars[index + 2..close].iter().collect();
            spans.extend(inline_styled(&inner, base.add_modifier(Modifier::BOLD)));
            index = close + 2;
            continue;
        }
        if (ch == '*' || ch == '_')
            && let Some(close) = find_single(&chars, index + 1, ch)
            && emphasis_content_ok(&chars[index + 1..close])
            && (ch == '*' || word_boundary(&chars, index, close))
        {
            flush(&mut plain, &mut spans);
            let inner: String = chars[index + 1..close].iter().collect();
            spans.extend(inline_styled(&inner, base.add_modifier(Modifier::ITALIC)));
            index = close + 1;
            continue;
        }
        if ch == '['
            && let Some(label_end) = find_char(&chars, index + 1, ']')
            && chars.get(label_end + 1) == Some(&'(')
            && let Some(url_end) = find_char(&chars, label_end + 2, ')')
            && label_end > index + 1
        {
            let label: String = chars[index + 1..label_end].iter().collect();
            let url: String = chars[label_end + 2..url_end].iter().collect();
            if !url.is_empty() && !url.contains(char::is_whitespace) {
                flush(&mut plain, &mut spans);
                spans.extend(inline_styled(
                    &label,
                    base.add_modifier(Modifier::UNDERLINED),
                ));
                // The target stays visible: a terminal has no hover, and a link
                // whose destination is hidden is a link the reader cannot judge
                // — unless the label already is the destination.
                if label != url {
                    spans.push((format!(" ({url})"), base.patch(link_target_style())));
                }
                index = url_end + 1;
                continue;
            }
        }
        plain.push(ch);
        index += 1;
    }
    flush(&mut plain, &mut spans);
    spans
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from.min(chars.len())..]
        .iter()
        .position(|ch| *ch == needle)
        .map(|offset| from + offset)
}

/// The first lone `needle` at or after `from` — a doubled one is a bold
/// marker and belongs to the inner span, so `*a **b** c*` closes at the end.
fn find_single(chars: &[char], from: usize, needle: char) -> Option<usize> {
    let mut at = from;
    while at < chars.len() {
        if chars[at] == needle {
            if chars.get(at + 1) == Some(&needle) {
                at += 2;
                continue;
            }
            return Some(at);
        }
        at += 1;
    }
    None
}

/// The first `needle needle` at or after `from`.
fn find_pair(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&at| chars[at] == needle && chars[at + 1] == needle)
}

/// Emphasis needs content that does not start or end with whitespace — the
/// rule that keeps `2 * 3 * 4` arithmetic out of italics.
fn emphasis_content_ok(content: &[char]) -> bool {
    content.first().is_some_and(|ch| !ch.is_whitespace())
        && content.last().is_some_and(|ch| !ch.is_whitespace())
}

/// `_` emphasis only at word boundaries, so `snake_case_names` stay literal.
fn word_boundary(chars: &[char], open: usize, close: usize) -> bool {
    let before = open.checked_sub(1).map(|at| chars[at]);
    let after = chars.get(close + 1).copied();
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

fn link_target_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Width-aware (CJK = 2 cells) char wrapping that preserves span styles.
/// Continuation lines get `hanging` spaces of indent.
fn wrap_spans(spans: Vec<(String, Style)>, width: usize, hanging: usize) -> Vec<Line<'static>> {
    wrap_spans_raw(spans, width, hanging)
        .into_iter()
        .map(Line::from)
        .collect()
}

/// Core of [`wrap_spans`], returning bare span rows so table cells can wrap
/// without committing to whole transcript [`Line`]s.
fn wrap_spans_raw(
    spans: Vec<(String, Style)>,
    width: usize,
    hanging: usize,
) -> Vec<Vec<Span<'static>>> {
    let hanging = hanging.min(width.saturating_sub(4));
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for (text, style) in spans {
        let mut buffer = String::new();
        for ch in text.chars() {
            let char_width = ch.width().unwrap_or(0);
            if current_width + char_width > width && current_width > 0 {
                if !buffer.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buffer), style));
                }
                lines.push(std::mem::take(&mut current));
                if hanging > 0 {
                    current.push(Span::raw(" ".repeat(hanging)));
                }
                current_width = hanging;
            }
            buffer.push(ch);
            current_width += char_width;
        }
        if !buffer.is_empty() {
            current.push(Span::styled(buffer, style));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn code_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn fence_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

fn table_chrome_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests;
