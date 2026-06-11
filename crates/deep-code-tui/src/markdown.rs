//! Markdown-lite rendering for assistant transcript cells.
//!
//! Two phases, following the reference design: a width-independent block
//! parse ([`parse_blocks`]) and a width-dependent line layout
//! ([`render_markdown`]). Resizing only re-runs the second phase. Only
//! fenced code, inline code, headings, and simple lists are styled —
//! everything else stays plain text.

use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Heading { level: u8, text: String },
    CodeBlock { language: Option<String>, lines: Vec<String> },
    ListItem { indent: usize, text: String },
    Paragraph { text: String },
    Blank,
}

#[must_use]
pub fn parse_blocks(text: &str) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let mut code: Option<(Option<String>, Vec<String>)> = None;

    for line in text.lines() {
        if let Some((_, code_lines)) = code.as_mut() {
            if line.trim_start().starts_with("```") {
                let (language, lines) = code.take().expect("code block in progress");
                blocks.push(MarkdownBlock::CodeBlock { language, lines });
            } else {
                code_lines.push(line.to_string());
            }
            continue;
        }

        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            let language = rest.trim();
            let language = (!language.is_empty()).then(|| language.to_string());
            code = Some((language, Vec::new()));
            continue;
        }
        if trimmed.is_empty() {
            blocks.push(MarkdownBlock::Blank);
            continue;
        }
        if let Some((level, rest)) = parse_heading(trimmed) {
            blocks.push(MarkdownBlock::Heading {
                level,
                text: rest.to_string(),
            });
            continue;
        }
        if let Some(rest) = parse_list_item(trimmed) {
            blocks.push(MarkdownBlock::ListItem {
                indent: line.len() - trimmed.len(),
                text: rest.to_string(),
            });
            continue;
        }
        blocks.push(MarkdownBlock::Paragraph {
            text: line.to_string(),
        });
    }

    // Unclosed fence (common while a reply was cancelled mid-stream): keep
    // the collected lines as a code block instead of dropping them.
    if let Some((language, lines)) = code {
        blocks.push(MarkdownBlock::CodeBlock { language, lines });
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
                let marker = match language {
                    Some(language) => format!("``` {language}"),
                    None => "```".to_string(),
                };
                out.push(Line::styled(marker, fence_style()));
                for code in lines {
                    out.extend(wrap_spans(
                        vec![(format!("  {code}"), code_style())],
                        width,
                        2,
                    ));
                }
                out.push(Line::styled("```", fence_style()));
            }
            MarkdownBlock::ListItem { indent, text } => {
                let pad = " ".repeat(indent.min(8));
                let mut spans = vec![(format!("{pad}• "), Style::default())];
                spans.extend(inline_spans(&text));
                out.extend(wrap_spans(spans, width, pad.len() + 2));
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
    let hanging = hanging.min(width.saturating_sub(4));
    let mut lines: Vec<Line<'static>> = Vec::new();
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
                lines.push(Line::from(std::mem::take(&mut current)));
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
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::default());
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn parses_fenced_code_with_language() {
        let blocks = parse_blocks("before\n```rust\nlet x = 1;\n```\nafter");
        assert_eq!(
            blocks,
            vec![
                MarkdownBlock::Paragraph {
                    text: "before".to_string()
                },
                MarkdownBlock::CodeBlock {
                    language: Some("rust".to_string()),
                    lines: vec!["let x = 1;".to_string()],
                },
                MarkdownBlock::Paragraph {
                    text: "after".to_string()
                },
            ]
        );
    }

    #[test]
    fn unclosed_fence_keeps_lines_without_panic() {
        let blocks = parse_blocks("```\nlet x = 1;\nlet y = 2;");
        assert_eq!(
            blocks,
            vec![MarkdownBlock::CodeBlock {
                language: None,
                lines: vec!["let x = 1;".to_string(), "let y = 2;".to_string()],
            }]
        );
        let lines = render_markdown("```\nlet x = 1;", 40);
        assert!(lines.iter().any(|line| line_text(line).contains("let x")));
    }

    #[test]
    fn classifies_headings_and_lists() {
        let blocks = parse_blocks("# Title\n- one\n* two\n3. three\nplain");
        assert_eq!(
            blocks[0],
            MarkdownBlock::Heading {
                level: 1,
                text: "Title".to_string()
            }
        );
        assert!(matches!(&blocks[1], MarkdownBlock::ListItem { text, .. } if text == "one"));
        assert!(matches!(&blocks[2], MarkdownBlock::ListItem { text, .. } if text == "two"));
        assert!(matches!(&blocks[3], MarkdownBlock::ListItem { text, .. } if text == "three"));
        assert!(matches!(&blocks[4], MarkdownBlock::Paragraph { text } if text == "plain"));
    }

    #[test]
    fn inline_code_becomes_styled_span() {
        let lines = render_markdown("run `cargo test` now", 80);
        assert_eq!(lines.len(), 1);
        let code_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "cargo test")
            .expect("inline code span");
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn unmatched_backtick_stays_literal() {
        let lines = render_markdown("a `broken", 80);
        assert_eq!(line_text(&lines[0]), "a `broken");
    }

    #[test]
    fn wraps_long_paragraphs_to_width() {
        let lines = render_markdown(&"a".repeat(25), 10);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| line_text(line).width() <= 10));
    }

    #[test]
    fn wraps_cjk_text_by_display_width() {
        let lines = render_markdown(&"中".repeat(10), 10);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line_text(line).width() <= 10));
    }

    #[test]
    fn list_wrap_uses_hanging_indent() {
        let lines = render_markdown(&format!("- {}", "x".repeat(30)), 16);
        assert!(lines.len() >= 2);
        assert!(line_text(&lines[0]).starts_with("• "));
        assert!(line_text(&lines[1]).starts_with("  "));
    }

    #[test]
    fn heading_renders_bold() {
        let lines = render_markdown("## 标题", 40);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert_eq!(line_text(&lines[0]), "## 标题");
    }
}
