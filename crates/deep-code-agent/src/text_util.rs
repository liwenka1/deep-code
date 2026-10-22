//! Small shared text helpers.

/// Truncate `text` to at most `max` characters (UTF-8 safe), appending an
/// ellipsis when anything was cut. Returns the input unchanged when it already
/// fits, so the ellipsis marks real elision only.
#[must_use]
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// `text` whole when it is at most `max` chars; past that its first `head` and
/// last `tail` chars with `marker(elided)` between them. Chars, not bytes, so
/// a multi-byte character is never split.
#[must_use]
pub(crate) fn clip_middle(
    text: &str,
    max: usize,
    head: usize,
    tail: usize,
    marker: impl FnOnce(usize) -> String,
) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(total.saturating_sub(tail)).collect();
    format!("{start}{}{end}", marker(total.saturating_sub(head + tail)))
}

#[cfg(test)]
mod tests {
    use super::{clip_middle, truncate_chars};

    #[test]
    fn clip_middle_keeps_both_ends_and_counts_the_cut() {
        assert_eq!(clip_middle("short", 10, 2, 2, |_| "…".to_string()), "short");
        assert_eq!(
            clip_middle("前前前中中中后后后", 6, 3, 3, |cut| format!(
                "[{cut}]"
            )),
            "前前前[3]后后后"
        );
    }

    #[test]
    fn keeps_short_text_and_marks_truncation() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello world", 5), "hello…");
    }

    #[test]
    fn counts_by_char_not_byte() {
        // Multi-byte chars must not be split mid-sequence.
        assert_eq!(truncate_chars("日本語テスト", 3), "日本語…");
    }
}
