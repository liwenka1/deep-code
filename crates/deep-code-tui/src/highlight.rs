//! Syntax highlighting for fenced code blocks.
//!
//! The transcript re-renders every frame while a reply streams, so raw
//! per-frame highlighting would burn a regex pass over every visible code
//! line. Instead results are memoized per line with the parser state chained
//! through the block: the cache key of line *n* folds in every line before
//! it, so an append-only stream hits the cache for the whole prefix and only
//! the new tail is highlighted. A changed line invalidates exactly itself
//! and everything after it — which is also the correct re-highlight set.
//!
//! Colors map to the terminal's capability: 24-bit where `COLORTERM` says
//! truecolor, quantized to xterm-256 otherwise. Theme backgrounds are
//! deliberately dropped — the canvas is the user's terminal, not the theme's.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::OnceLock;

use ratatui::prelude::{Color, Modifier, Style};
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

/// Bundled theme used for all highlighting (foreground colors only).
const THEME: &str = "base16-ocean.dark";

/// Cache lines kept before the memo is wiped wholesale. A wipe only costs
/// one full re-highlight of the visible blocks on the next frame.
const CACHE_CAP: usize = 16 * 1024;

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn highlighter() -> Option<&'static Highlighter<'static>> {
    static THEME_SLOT: OnceLock<Option<Theme>> = OnceLock::new();
    static HIGHLIGHTER: OnceLock<Option<Highlighter<'static>>> = OnceLock::new();
    HIGHLIGHTER
        .get_or_init(|| {
            THEME_SLOT
                .get_or_init(|| ThemeSet::load_defaults().themes.remove(THEME))
                .as_ref()
                .map(Highlighter::new)
        })
        .as_ref()
}

struct CachedLine {
    spans: Vec<(String, Style)>,
    parse: ParseState,
    highlight: HighlightState,
}

thread_local! {
    static CACHE: RefCell<HashMap<u64, CachedLine>> = RefCell::new(HashMap::new());
}

#[cfg(test)]
thread_local! {
    static MISSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note_miss() {
    #[cfg(test)]
    MISSES.with(|misses| misses.set(misses.get() + 1));
}

/// Highlight a fenced block's lines into styled spans (one row per line).
/// Returns `None` when the language token is unknown or the highlighter is
/// unavailable — the caller keeps its plain single-color style.
pub fn highlight_block(language: &str, lines: &[String]) -> Option<Vec<Vec<(String, Style)>>> {
    let syntaxes = syntax_set();
    let syntax = syntaxes.find_syntax_by_token(language)?;
    let highlighter = highlighter()?;

    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > CACHE_CAP {
            cache.clear();
        }

        // Chained keys: key(n) = hash(key(n-1), line(n)), seeded with the
        // syntax name so identical text in different languages can't collide.
        let mut keys = Vec::with_capacity(lines.len());
        let mut key = {
            let mut hasher = DefaultHasher::new();
            syntax.name.hash(&mut hasher);
            hasher.finish()
        };
        for line in lines {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            line.hash(&mut hasher);
            key = hasher.finish();
            keys.push(key);
        }

        let first_miss = keys
            .iter()
            .position(|key| !cache.contains_key(key))
            .unwrap_or(lines.len());

        let mut out = Vec::with_capacity(lines.len());
        for key in &keys[..first_miss] {
            out.push(cache[key].spans.clone());
        }

        let (mut parse, mut highlight) = if first_miss == 0 {
            (
                ParseState::new(syntax),
                HighlightState::new(highlighter, ScopeStack::new()),
            )
        } else {
            let previous = &cache[&keys[first_miss - 1]];
            (previous.parse.clone(), previous.highlight.clone())
        };

        for (index, line) in lines.iter().enumerate().skip(first_miss) {
            note_miss();
            let spans = highlight_one_line(line, &mut parse, &mut highlight, highlighter)?;
            cache.insert(
                keys[index],
                CachedLine {
                    spans: spans.clone(),
                    parse: parse.clone(),
                    highlight: highlight.clone(),
                },
            );
            out.push(spans);
        }
        Some(out)
    })
}

/// One line through the parser + highlighter, advancing both states.
/// The grammars expect the trailing newline; it never reaches the output.
fn highlight_one_line(
    line: &str,
    parse: &mut ParseState,
    highlight: &mut HighlightState,
    highlighter: &Highlighter<'_>,
) -> Option<Vec<(String, Style)>> {
    let with_newline = format!("{line}\n");
    let ops = parse.parse_line(&with_newline, syntax_set()).ok()?;
    let mut spans = Vec::new();
    for (style, text) in HighlightIterator::new(highlight, &ops, &with_newline, highlighter) {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        spans.push((text.to_string(), map_style(style)));
    }
    if spans.is_empty() {
        spans.push((String::new(), Style::default()));
    }
    Some(spans)
}

fn map_style(style: syntect::highlighting::Style) -> Style {
    // Foreground only: a theme background painted under the text would clash
    // with whatever the user's terminal background actually is.
    let mut mapped = Style::default().fg(map_color(style.foreground, truecolor()));
    if style.font_style.contains(FontStyle::BOLD) {
        mapped = mapped.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        mapped = mapped.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        mapped = mapped.add_modifier(Modifier::UNDERLINED);
    }
    mapped
}

fn map_color(color: syntect::highlighting::Color, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb(color.r, color.g, color.b)
    } else {
        Color::Indexed(xterm256_index(color.r, color.g, color.b))
    }
}

fn truecolor() -> bool {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|value| colorterm_signals_truecolor(&value))
    })
}

fn colorterm_signals_truecolor(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("truecolor") || value.contains("24bit")
}

/// Nearest xterm-256 index: the 6×6×6 color cube (16–231) against the grey
/// ramp (232–255), whichever is closer in RGB distance.
fn xterm256_index(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_step = |value: u8| -> usize {
        let mut best = 0;
        for (index, step) in STEPS.iter().enumerate() {
            let current = (i32::from(value) - i32::from(STEPS[best])).abs();
            if (i32::from(value) - i32::from(*step)).abs() < current {
                best = index;
            }
        }
        best
    };
    let (ri, gi, bi) = (nearest_step(r), nearest_step(g), nearest_step(b));
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube = (STEPS[ri], STEPS[gi], STEPS[bi]);

    let grey_level = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let grey_step = usize::from(grey_level.saturating_sub(3) / 10).min(23);
    let grey_value = grey_step * 10 + 8;
    let grey_index = 232 + grey_step;
    let grey_value = u8::try_from(grey_value).unwrap_or(u8::MAX);
    let grey = (grey_value, grey_value, grey_value);

    let distance = |candidate: (u8, u8, u8)| -> i32 {
        let dr = i32::from(r) - i32::from(candidate.0);
        let dg = i32::from(g) - i32::from(candidate.1);
        let db = i32::from(b) - i32::from(candidate.2);
        dr * dr + dg * dg + db * db
    };
    let index = if distance(grey) < distance(cube) {
        grey_index
    } else {
        cube_index
    };
    u8::try_from(index).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn misses() -> usize {
        MISSES.with(std::cell::Cell::get)
    }

    #[test]
    fn known_language_gets_multiple_colors_and_exact_text() {
        let lines = vec!["fn main() { let s = \"hi\"; }".to_string()];
        let rows = highlight_block("rust", &lines).expect("rust grammar is bundled");
        let joined: String = rows[0].iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, lines[0], "highlighting must not alter the text");
        let colors: std::collections::HashSet<_> =
            rows[0].iter().filter_map(|(_, style)| style.fg).collect();
        assert!(colors.len() > 1, "expected several colors, got {colors:?}");
    }

    #[test]
    fn unknown_language_token_falls_back() {
        assert!(highlight_block("no-such-lang", &["x".to_string()]).is_none());
    }

    #[test]
    fn parser_state_carries_across_lines() {
        // The unterminated string on line 1 must still color line 2 as a
        // string — proof the state chain survives the per-line cache.
        let lines = vec!["let s = \"abc".to_string(), "def\";".to_string()];
        let rows = highlight_block("rust", &lines).expect("rust grammar is bundled");
        let string_color = rows[0]
            .iter()
            .find(|(text, _)| text.contains("abc"))
            .and_then(|(_, style)| style.fg)
            .expect("string literal span on line 1");
        let continuation_color = rows[1]
            .iter()
            .find(|(text, _)| text.contains("def"))
            .and_then(|(_, style)| style.fg)
            .expect("continuation span on line 2");
        assert_eq!(string_color, continuation_color);
    }

    #[test]
    fn streaming_append_recomputes_only_the_new_tail() {
        let mut lines: Vec<String> = vec!["fn a() {}".into(), "fn b() {}".into()];
        highlight_block("rust", &lines).expect("highlight");
        let after_first = misses();
        assert!(after_first >= lines.len());

        highlight_block("rust", &lines).expect("highlight");
        assert_eq!(misses(), after_first, "re-render must be pure cache hits");

        lines.push("fn c() {}".into());
        highlight_block("rust", &lines).expect("highlight");
        assert_eq!(
            misses(),
            after_first + 1,
            "append must only highlight the appended line"
        );
    }

    #[test]
    fn colorterm_detection_matches_convention() {
        assert!(colorterm_signals_truecolor("truecolor"));
        assert!(colorterm_signals_truecolor("24bit"));
        assert!(!colorterm_signals_truecolor("xterm-256color"));
    }

    #[test]
    fn xterm256_maps_primaries_and_greys_sanely() {
        assert_eq!(xterm256_index(0, 0, 0), 16, "black lands in the cube");
        assert_eq!(xterm256_index(255, 0, 0), 196, "pure red lands in the cube");
        assert_eq!(
            xterm256_index(128, 128, 128),
            244,
            "mid grey lands on the grey ramp"
        );
    }

    #[test]
    fn theme_and_grammars_are_bundled() {
        assert!(highlighter().is_some(), "bundled theme must resolve");
        assert!(syntax_set().find_syntax_by_token("rust").is_some());
        assert!(syntax_set().find_syntax_by_token("py").is_some());
        assert!(syntax_set().find_syntax_by_token("json").is_some());
    }
}
