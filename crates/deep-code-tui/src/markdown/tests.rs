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

#[test]
fn parses_pipe_table_with_alignments() {
    let blocks = parse_blocks("| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |");
    assert_eq!(
        blocks,
        vec![MarkdownBlock::Table {
            alignments: vec![ColumnAlign::Left, ColumnAlign::Center, ColumnAlign::Right],
            header: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            rows: vec![vec!["1".to_string(), "2".to_string(), "3".to_string()]],
        }]
    );
}

#[test]
fn header_without_separator_stays_paragraph() {
    // The separator has not streamed in yet — nothing snaps to a table.
    let blocks = parse_blocks("| a | b |");
    assert!(matches!(blocks[0], MarkdownBlock::Paragraph { .. }));
    let blocks = parse_blocks("| a | b |\nplain");
    assert!(
        blocks
            .iter()
            .all(|block| !matches!(block, MarkdownBlock::Table { .. }))
    );
}

#[test]
fn separator_column_count_must_match_header() {
    let blocks = parse_blocks("| a | b |\n|---|\n| 1 | 2 |");
    assert!(
        blocks
            .iter()
            .all(|block| !matches!(block, MarkdownBlock::Table { .. }))
    );
}

#[test]
fn escaped_pipe_stays_literal_in_cell() {
    let blocks = parse_blocks("| a \\| b | c |\n|---|---|");
    match &blocks[0] {
        MarkdownBlock::Table { header, .. } => assert_eq!(header[0], "a | b"),
        other => panic!("expected table, got {other:?}"),
    }
}

#[test]
fn ragged_rows_normalize_to_header_width() {
    let blocks = parse_blocks("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |");
    match &blocks[0] {
        MarkdownBlock::Table { rows, .. } => {
            assert_eq!(rows[0], vec!["1".to_string(), String::new()]);
            assert_eq!(rows[1], vec!["1".to_string(), "2".to_string()]);
        }
        other => panic!("expected table, got {other:?}"),
    }
}

#[test]
fn table_ends_at_first_non_row_line() {
    let blocks = parse_blocks("| a |\n|---|\n| 1 |\n\nafter");
    assert!(matches!(blocks[0], MarkdownBlock::Table { .. }));
    assert_eq!(blocks[1], MarkdownBlock::Blank);
    assert!(matches!(&blocks[2], MarkdownBlock::Paragraph { text } if text == "after"));
}

#[test]
fn table_renders_padded_columns_with_rule() {
    let lines = render_markdown("| name | n |\n|------|---|\n| ab | 1 |\n| a | 22 |", 40);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(texts[0], "name │ n");
    assert_eq!(texts[1], format!("{}─┼─{}", "─".repeat(4), "─".repeat(2)));
    assert_eq!(texts[2], "ab   │ 1");
    assert_eq!(texts[3], "a    │ 22");
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
    );
}

#[test]
fn right_and_center_alignment_pad_before_content() {
    let lines = render_markdown("| aaa | bbb |\n|---:|:--:|\n| 1 | 2 |", 40);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(texts[2], "  1 │  2");
}

#[test]
fn cjk_cells_align_by_display_width() {
    let lines = render_markdown("| 名字 | n |\n|---|---|\n| ab | 1 |", 40);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(texts[0], "名字 │ n");
    assert_eq!(texts[2], "ab   │ 1");
}

#[test]
fn overwide_cells_wrap_inside_their_column() {
    let table = format!("| a | b |\n|---|---|\n| {} | x |", "y".repeat(30));
    let lines = render_markdown(&table, 24);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert!(texts.iter().all(|text| text.width() <= 24));
    assert!(texts.len() >= 4, "row should wrap to two lines: {texts:?}");
    assert!(texts[2].starts_with(&"y".repeat(20)));
    assert!(texts[2].contains("│ x"));
    assert!(texts[3].starts_with(&"y".repeat(10)));
}

#[test]
fn impossibly_narrow_table_falls_back_to_plain_text() {
    let lines = render_markdown(
        "| aaa | bbb | ccc | ddd |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |",
        10,
    );
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert!(texts.iter().any(|text| text.contains("aaa")));
    assert!(texts.iter().all(|text| !text.contains('│')));
}

#[test]
fn fenced_code_with_known_language_gets_highlighted_spans() {
    let lines = render_markdown("```rust\nfn main() { let s = \"hi\"; }\n```", 60);
    let colors: std::collections::HashSet<_> = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect();
    // Fence markers are DarkGray; real highlighting adds several more.
    assert!(
        colors.len() > 2,
        "expected highlight colors, got {colors:?}"
    );
}

#[test]
fn fenced_code_with_unknown_language_keeps_plain_style() {
    let lines = render_markdown("```zzz-unknown\nplain text\n```", 60);
    let code_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref().contains("plain text"))
        .expect("code line span");
    assert_eq!(code_span.style.fg, Some(Color::Cyan));
}

#[test]
fn inline_code_inside_cells_keeps_its_style() {
    let lines = render_markdown("| cmd | note |\n|---|---|\n| `ls -la` | list |", 40);
    let code_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "ls -la")
        .expect("inline code span inside a table cell");
    assert_eq!(code_span.style.fg, Some(Color::Cyan));
}
