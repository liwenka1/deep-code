//! Markdown-lite rendering for assistant transcript cells.
//!
//! Two phases, following the reference design: a width-independent block
//! parse ([`parse_blocks`]) and a width-dependent line layout
//! ([`render_markdown`]). Resizing only re-runs the second phase. Only
//! fenced code, inline code, headings, simple lists, and pipe tables are
//! styled — everything else stays plain text.

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
        if let Some(rest) = parse_list_item(trimmed) {
            blocks.push(MarkdownBlock::ListItem {
                indent: line.len() - trimmed.len(),
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
            MarkdownBlock::ListItem { indent, text } => {
                let pad = " ".repeat(indent.min(8));
                let mut spans = vec![(format!("{pad}• "), Style::default())];
                spans.extend(inline_spans(&text));
                out.extend(wrap_spans(spans, width, pad.len() + 2));
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

fn parse_list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest);
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        line[digits..].strip_prefix(". ")
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

/// Split a paragraph into plain and `inline code` spans. Unmatched backticks
/// stay literal.
fn inline_spans(text: &str) -> Vec<(String, Style)> {
    let mut spans = Vec::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find('`') else {
            if !rest.is_empty() {
                spans.push((rest.to_string(), Style::default()));
            }
            break;
        };
        let Some(end) = rest[start + 1..].find('`') else {
            if !rest.is_empty() {
                spans.push((rest.to_string(), Style::default()));
            }
            break;
        };
        if start > 0 {
            spans.push((rest[..start].to_string(), Style::default()));
        }
        spans.push((rest[start + 1..start + 1 + end].to_string(), code_style()));
        rest = &rest[start + end + 2..];
    }
    if spans.is_empty() {
        spans.push((String::new(), Style::default()));
    }
    spans
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
