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

/// A numbered list keeps its numbers as written — a restart at 10 stays 10 —
/// and continuation rows hang under the text, not under the number.
#[test]
fn numbered_lists_keep_their_numbers() {
    let blocks = parse_blocks("1. first\n2. second\n- dash\n10. tenth");
    assert!(matches!(
        &blocks[0],
        MarkdownBlock::ListItem { number: Some(number), text, .. }
            if number == "1." && text == "first"
    ));
    assert!(matches!(
        &blocks[2],
        MarkdownBlock::ListItem { number: None, .. }
    ));
    assert!(matches!(
        &blocks[3],
        MarkdownBlock::ListItem { number: Some(number), .. } if number == "10."
    ));

    let lines = render_markdown("1. first\n2. second", 40);
    assert_eq!(line_text(&lines[0]), "1. first");
    assert_eq!(line_text(&lines[1]), "2. second");

    let wrapped = render_markdown(&format!("12. {}", "x".repeat(30)), 16);
    assert!(line_text(&wrapped[0]).starts_with("12. x"));
    assert!(
        line_text(&wrapped[1]).starts_with("    x"),
        "continuation hangs under the text: {:?}",
        line_text(&wrapped[1])
    );
}

/// Bold, italic and links get their modifiers; a link's target stays visible
/// after its label. Markers without a closer, emphasis around spaces
/// (arithmetic) and underscores inside identifiers stay literal.
#[test]
fn inline_bold_italic_and_links_are_styled() {
    let lines = render_markdown(
        "say **loud** and *soft* or _soft_ then [docs](https://x.y/d) and `code`",
        120,
    );
    let spans: Vec<(String, Style)> = lines[0]
        .spans
        .iter()
        .map(|span| (span.content.to_string(), span.style))
        .collect();
    let style_of = |needle: &str| {
        spans
            .iter()
            .find(|(text, _)| text == needle)
            .map(|(_, style)| *style)
            .unwrap_or_else(|| panic!("no span {needle:?} in {spans:?}"))
    };
    assert!(style_of("loud").add_modifier.contains(Modifier::BOLD));
    assert!(style_of("soft").add_modifier.contains(Modifier::ITALIC));
    assert!(style_of("docs").add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(style_of("code").fg, Some(Color::Cyan));
    assert_eq!(
        line_text(&lines[0]),
        "say loud and soft or soft then docs (https://x.y/d) and code"
    );

    for literal in [
        "2 * 3 * 4",
        "a ** b",
        "snake_case_name here",
        "**unclosed",
        "[not a link] (x)",
        "[spaced](a b)",
    ] {
        let lines = render_markdown(literal, 80);
        assert_eq!(
            line_text(&lines[0]),
            literal,
            "{literal:?} must stay literal"
        );
    }

    // Nesting: bold inside italic keeps both; code inside bold stays code.
    let lines = render_markdown("*a **b** c* and **`x`**", 80);
    let spans: Vec<(String, Style)> = lines[0]
        .spans
        .iter()
        .map(|span| (span.content.to_string(), span.style))
        .collect();
    let b = spans.iter().find(|(text, _)| text == "b").unwrap().1;
    assert!(b.add_modifier.contains(Modifier::BOLD | Modifier::ITALIC));
    let x = spans.iter().find(|(text, _)| text == "x").unwrap().1;
    assert_eq!(x.fg, Some(Color::Cyan));
    assert!(x.add_modifier.contains(Modifier::BOLD));

    // A link whose label is its own target prints the target once.
    let lines = render_markdown("[https://x.y](https://x.y)", 80);
    assert_eq!(line_text(&lines[0]), "https://x.y");
}

/// Any paragraph renders in time linear in its length, without a stack frame
/// per character: a reply is re-rendered every frame while it streams, and a
/// model stuck repeating `*` or `_` produced a line the recursive parser took
/// quadratic time over and, past a few thousand characters on a thread with
/// the Windows main-thread stack, overflowed on — aborting with the terminal
/// still raw, and again on every `/resume` of that session.
#[test]
fn any_paragraph_renders_in_linear_time_on_a_small_stack() {
    let n = 100_000;
    let inputs = vec![
        "*".repeat(n),
        "_".repeat(n),
        "*a".repeat(n / 2),
        "**a".repeat(n / 3),
        "*_".repeat(n / 2),
        "a*b".repeat(n / 3),
        "[".repeat(n),
        "[a]".repeat(n / 3),
        "[a](".repeat(n / 4),
        "`".repeat(n),
        format!("{}x{}", "*a ".repeat(n / 6), " a*".repeat(n / 6)),
        format!("{}x", "_a ".repeat(n / 3)),
        // Closers that find no opener of their own marker above a long stack
        // of the other one: each must not walk the whole stack again.
        format!("{}{}", "_a ".repeat(n / 2), "a* ".repeat(n / 2)),
    ];
    // Each takes tens of milliseconds unoptimized; the quadratic parser took
    // seconds on the bracket lines alone and overflowed on the marker ones.
    let rendering = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            inputs
                .iter()
                .map(|input| {
                    let started = std::time::Instant::now();
                    let lines = render_markdown(input, 100);
                    assert!(!lines.is_empty());
                    (input.chars().take(6).collect::<String>(), started.elapsed())
                })
                .collect::<Vec<_>>()
        })
        .unwrap();
    let timings = rendering
        .join()
        .expect("rendering must not need a deep stack");
    for (input, took) in timings {
        assert!(
            took < std::time::Duration::from_secs(2),
            "{input:?}… took {took:?}"
        );
    }
}

/// Delimiter runs pair the way Markdown pairs them: a run is one delimiter, so
/// the second `*` of a `**` that found no partner is not an italic opener of
/// its own, and runs whose lengths add up to a multiple of three do not pair
/// when one could go either way. Bold after punctuation in Chinese prose
/// still works.
#[test]
fn delimiter_runs_pair_the_way_markdown_pairs_them() {
    for literal in [
        "edit src/**/*.rs",
        "match **/*.rs files",
        "use *.rs and *.toml",
        "a ** b",
        "****",
        "a_b_c",
    ] {
        let lines = render_markdown(literal, 80);
        assert_eq!(
            line_text(&lines[0]),
            literal,
            "{literal:?} must stay literal"
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.is_empty()),
            "{literal:?}: {:?}",
            lines[0].spans
        );
    }

    let spans_of = |text: &str| -> Vec<(String, Style)> {
        render_markdown(text, 80)[0]
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect()
    };
    let modifiers = |spans: &[(String, Style)], needle: &str| {
        spans
            .iter()
            .find(|(text, _)| text == needle)
            .map(|(_, style)| style.add_modifier)
            .unwrap_or_else(|| panic!("no span {needle:?} in {spans:?}"))
    };

    let spans = spans_of("***key*** done");
    assert_eq!(
        spans
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>(),
        "key done"
    );
    assert!(modifiers(&spans, "key").contains(Modifier::BOLD | Modifier::ITALIC));

    let spans = spans_of("**注意：**这里要改");
    assert_eq!(
        spans
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>(),
        "注意：这里要改"
    );
    assert!(modifiers(&spans, "注意：").contains(Modifier::BOLD));

    let spans = spans_of("**a**b**c**");
    assert!(modifiers(&spans, "a").contains(Modifier::BOLD));
    assert!(!modifiers(&spans, "b").contains(Modifier::BOLD));
    assert!(modifiers(&spans, "c").contains(Modifier::BOLD));

    let spans = spans_of("***x** y*");
    assert!(modifiers(&spans, "x").contains(Modifier::BOLD | Modifier::ITALIC));
    assert_eq!(modifiers(&spans, " y"), Modifier::ITALIC);
}
