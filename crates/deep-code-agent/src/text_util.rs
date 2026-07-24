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

#[cfg(test)]
mod tests {
    use super::truncate_chars;

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
