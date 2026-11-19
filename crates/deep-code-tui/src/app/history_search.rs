//! Ctrl+R: reverse search through the prompt history, readline style.
//!
//! While the search is open the composer shows the newest prompt that
//! contains the query, and the status row shows the query and where the
//! match sits — ahead of anything else that row could be showing. Typing
//! narrows, Ctrl+R (or ↑) steps to the next older match, Enter keeps the
//! shown prompt in the composer for editing or sending, Esc puts the composer
//! back exactly as it was. Any other key keeps the match and goes on to do its
//! usual job, so `Ctrl+R fix Ctrl+A` lands at the start of the recalled
//! prompt. A kept match is where `↑` / `↓` go on from, and the draft is one
//! `↓` past the newest entry, as after any recall; a search that never matched
//! gives the draft back.
//!
//! Accepting does not send: a recalled prompt re-runs an agent, which is a
//! heavier thing than re-running a shell command, so it gets a look first.

use super::{App, HistoryPrompt};
use deep_code_agent::i18n::TextId;
use deep_code_agent::neutralize_char_into;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The open search: what the user typed, what the composer held when it
/// opened, and which history entry is currently shown.
#[derive(Debug, Clone)]
pub(crate) struct HistorySearch {
    query: String,
    /// The composer when the search opened: the draft being typed, or a prompt
    /// an `↑` walk had recalled.
    draft: HistoryPrompt,
    /// Where that `↑` walk stood, if one was under way. The draft the walk
    /// parked stays parked (`history_draft`) through the search.
    walk: Option<usize>,
    /// Index into `prompt_history` of the shown match; `None` while nothing
    /// matches (the composer then keeps the last match, like readline).
    shown: Option<usize>,
    /// The match the composer holds: the shown one, or the last one shown
    /// before the query stopped matching. `None` until something matched.
    in_composer: Option<usize>,
    /// Localized status with `{query}` still present so the renderer can
    /// place the cursor after the editable part.
    status: String,
    short_count: Option<String>,
    layout: Option<HistorySearchLayout>,
}

#[derive(Debug, Clone, Copy)]
enum SearchResultDisplay {
    Full,
    Compact,
    CompactWithMarker,
    ShortCount,
    QuoteOnly,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchResultWidths {
    full: usize,
    compact: usize,
    marker: Option<usize>,
    short_count: Option<usize>,
}

#[derive(Debug, Clone)]
struct HistorySearchLayout {
    available: usize,
    inline_available: usize,
    separate_available: Option<usize>,
    full_prefix: String,
    compact_prefix: &'static str,
    result_widths: SearchResultWidths,
    label: String,
    visible_query: String,
    result: SearchResultDisplay,
}

impl HistorySearchLayout {
    fn new(
        raw_query: &str,
        prefix: &str,
        compact: &'static str,
        result_widths: SearchResultWidths,
        (inline_available, separate_available): (usize, Option<usize>),
    ) -> Self {
        // Show line breaks and tabs on the single display row. Matching
        // uses the raw query, including its original whitespace.
        let mut query = String::with_capacity(raw_query.len());
        for ch in raw_query.chars() {
            let visible = match ch {
                '\n' => '↵',
                '\t' => '⇥',
                _ => ch,
            };
            neutralize_char_into(&mut query, visible);
        }

        // Reserve a cell for the caret and enough room for the entire query,
        // or the ellipsis and the last visible grapheme.
        let ellipsis = "…";
        let ellipsis_width = ellipsis.width();
        let query_width: usize = query.graphemes(true).map(UnicodeWidthStr::width).sum();
        let last_width = query
            .graphemes(true)
            .rev()
            .map(UnicodeWidthStr::width)
            .find(|&width| width > 0)
            .unwrap_or_default();
        let query_min = query_width.min(ellipsis_width + last_width);
        let result_reserve = result_widths.compact.max(1);
        let inline_result_fits = [prefix, compact]
            .iter()
            .any(|label| label.width() + query_min + result_reserve <= inline_available)
            || (query_width > 0
                && result_widths.marker.is_some_and(|width| {
                    "?".width() + query_min + width.max(1) <= inline_available
                }));
        let available = separate_available
            .filter(|_| inline_available <= 1 || !inline_result_fits)
            .unwrap_or(inline_available);
        let query_min = if query_min < available {
            query_min
        } else {
            ellipsis_width
        };
        let fits_complete = |label: &str, result_width: usize| {
            label.width() + query_width + result_width.max(1) <= available
        };
        let fits_tail = |label: &str, reserve| label.width() + query_min + reserve <= available;
        // Keep the result visible alongside a clipped query whenever the
        // search label, result, and query tail fit together.
        let (label, result) = if fits_complete(prefix, result_widths.full) {
            (prefix, SearchResultDisplay::Full)
        } else if fits_complete(prefix, result_widths.compact) {
            (prefix, SearchResultDisplay::Compact)
        } else if fits_complete(compact, result_widths.compact) {
            (compact, SearchResultDisplay::Compact)
        } else if fits_tail(prefix, result_reserve) {
            (prefix, SearchResultDisplay::Compact)
        } else if fits_tail(compact, result_reserve) {
            (compact, SearchResultDisplay::Compact)
        } else if query_width > 0
            && result_widths
                .marker
                .is_some_and(|width| fits_tail("?", width.max(1)))
        {
            ("?", SearchResultDisplay::CompactWithMarker)
        } else if result_widths
            .short_count
            .is_some_and(|width| fits_tail(compact, width))
        {
            (compact, SearchResultDisplay::ShortCount)
        } else if fits_tail(prefix, 1) {
            (prefix, SearchResultDisplay::QuoteOnly)
        } else if fits_tail(compact, 1) {
            (compact, SearchResultDisplay::QuoteOnly)
        } else if fits_tail("?", 1) {
            ("?", SearchResultDisplay::Hidden)
        } else {
            ("", SearchResultDisplay::Hidden)
        };

        let reserved = match result {
            SearchResultDisplay::Full => result_widths.full.max(1),
            SearchResultDisplay::Compact => result_reserve,
            SearchResultDisplay::CompactWithMarker => result_widths.marker.unwrap_or(1).max(1),
            SearchResultDisplay::ShortCount => result_widths.short_count.unwrap_or(1).max(1),
            SearchResultDisplay::QuoteOnly | SearchResultDisplay::Hidden => 1,
        };
        let mut budget = available.saturating_sub(label.width() + reserved);
        let mut visible_query = String::new();
        if query_width > budget && budget >= ellipsis_width {
            visible_query.push_str(ellipsis);
            budget -= ellipsis_width;
        }
        let mut remaining = budget;
        let mut start = query.len();
        for (index, grapheme) in query.grapheme_indices(true).rev() {
            let Some(left) = remaining.checked_sub(grapheme.width()) else {
                break;
            };
            remaining = left;
            start = index;
        }
        if remaining < budget {
            visible_query.push_str(&query[start..]);
        }

        Self {
            available,
            inline_available,
            separate_available,
            full_prefix: prefix.into(),
            compact_prefix: compact,
            result_widths,
            label: label.into(),
            visible_query,
            result,
        }
    }
}

fn split_status(status: &str) -> (&str, &str) {
    status.split_once("{query}").unwrap_or(("", status))
}

impl App {
    #[must_use]
    pub(crate) fn history_search_open(&self) -> bool {
        self.history_search.is_some()
    }

    pub(crate) fn history_search_focused(&self) -> bool {
        self.history_search_open()
            && self.pending_approval.is_none()
            && !self.resume_picker_open()
            && !self.model_picker_open()
    }

    /// The status text before the query, the editable query, and the text after it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn history_search_status(&self) -> Option<(&str, &str, &str)> {
        let search = self.history_search.as_ref()?;
        let (before, after) = split_status(&search.status);
        Some((before, &search.query, after))
    }

    /// Reserve a separate row when the badges leave no editable space or when
    /// the visible preview would otherwise lose its no-match result.
    pub(crate) fn prepare_history_search_field(
        &mut self,
        inline_available: usize,
        full_width: usize,
    ) -> bool {
        self.history_search_field(inline_available, Some(full_width))
            .is_some_and(|(layout, _)| layout.available != inline_available)
    }

    /// The label, visible query, and result details for the available columns.
    pub(crate) fn history_search_display(
        &mut self,
        available: usize,
    ) -> Option<(&str, &str, &str)> {
        let (layout, result) = self.history_search_field(available, None)?;
        Some((&layout.label, &layout.visible_query, result))
    }

    /// Fit complete controls into the columns left after the query and result.
    pub(crate) fn history_search_hints(&self, available: usize) -> Option<String> {
        let search = self.history_search.as_ref()?;
        let prefix = " — ";
        let short_cancel = "Esc";
        let mut remaining = available.checked_sub(prefix.width())?;
        if remaining < short_cancel.width() {
            return None;
        }
        let controls: &[TextId] = if search.shown.is_some() {
            &[
                TextId::HistorySearchHintOlder,
                TextId::HistorySearchHintKeep,
            ]
        } else if self.prompt_history.is_empty() {
            &[]
        } else {
            &[TextId::HistorySearchHintWiden]
        };
        let separator = self.tr(TextId::HistorySearchHintSeparator);
        let mut hints = prefix.to_string();
        for &id in controls {
            let hint = self.tr(id);
            let width = hint.width() + separator.width();
            // Keep a way back to the draft alongside the other controls.
            if width + short_cancel.width() <= remaining {
                hints.push_str(hint);
                hints.push_str(separator);
                remaining -= width;
            }
        }
        let cancel = self.tr(TextId::HistorySearchHintCancel);
        hints.push_str(if cancel.width() <= remaining {
            cancel
        } else {
            short_cancel
        });
        Some(hints)
    }

    fn history_search_field(
        &mut self,
        available: usize,
        full_width: Option<usize>,
    ) -> Option<(&HistorySearchLayout, &str)> {
        let compact = self.tr(TextId::HistorySearchPrefixShort);
        let empty_result = self.tr(TextId::HistorySearchEmptyShort);
        let empty_history = self.prompt_history.is_empty();
        let search = self.history_search.as_mut()?;
        let stale_preview = search.shown.is_none() && search.in_composer.is_some();
        let separate_available = full_width.filter(|_| available <= 1 || stale_preview);
        let (prefix, suffix) = split_status(&search.status);
        let compact_result = if empty_history { empty_result } else { suffix };
        // The question mark leaves the query unquoted. Retain the readable
        // search label on narrow terminals when there is no history yet.
        let marker_result = compact_result.strip_prefix('\'').unwrap_or(compact_result);
        let result_widths = SearchResultWidths {
            full: suffix.width(),
            compact: compact_result.width(),
            marker: (!empty_history).then(|| marker_result.width()),
            short_count: search.short_count.as_deref().map(UnicodeWidthStr::width),
        };
        if search.layout.as_ref().is_none_or(|layout| {
            let width_changed = if full_width.is_some() {
                layout.inline_available != available
                    || layout.separate_available != separate_available
            } else {
                layout.available != available
            };
            width_changed
                || layout.full_prefix != prefix
                || layout.compact_prefix != compact
                || layout.result_widths != result_widths
        }) {
            search.layout = Some(HistorySearchLayout::new(
                &search.query,
                prefix,
                compact,
                result_widths,
                (available, separate_available),
            ));
        }
        let layout = search.layout.as_ref()?;
        let result = match layout.result {
            SearchResultDisplay::Full => suffix,
            SearchResultDisplay::Compact => compact_result,
            SearchResultDisplay::CompactWithMarker => marker_result,
            SearchResultDisplay::ShortCount => search.short_count.as_deref().unwrap_or_default(),
            SearchResultDisplay::QuoteOnly => "'",
            SearchResultDisplay::Hidden => "",
        };
        Some((layout, result))
    }

    /// Ctrl+R: open the search over the current draft, or step to the next
    /// older match when it is already open.
    pub(crate) fn history_search_start(&mut self) {
        if self.history_search.is_some() {
            self.history_search_older();
            return;
        }
        self.close_completion();
        self.history_search = Some(HistorySearch {
            query: String::new(),
            draft: HistoryPrompt {
                text: std::mem::take(&mut self.input),
                images: std::mem::take(&mut self.attached_images),
            },
            walk: self.history_cursor.take(),
            shown: None,
            in_composer: None,
            status: String::new(),
            short_count: None,
            layout: None,
        });
        self.input_cursor = 0;
        self.history_search_refresh(self.prompt_history.len());
    }

    pub(crate) fn history_search_push(&mut self, ch: char) {
        self.history_search_append(ch.encode_utf8(&mut [0; 4]));
    }

    pub(crate) fn history_search_append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        search.query.push_str(text);
        search.layout = None;
        // A narrower query is searched from the newest entry again: the match
        // the broader one landed on may no longer qualify, and the newest
        // qualifying one is the readline answer.
        self.history_search_refresh(self.prompt_history.len());
    }

    pub(crate) fn history_search_backspace(&mut self) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        if search.query.pop().is_some() {
            search.layout = None;
        }
        self.history_search_refresh(self.prompt_history.len());
    }

    /// The next match older than the one shown.
    pub(crate) fn history_search_older(&mut self) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let from = search.shown.unwrap_or(self.prompt_history.len());
        self.history_search_refresh(from);
    }

    /// Keep what the composer shows and close the search. A match becomes the
    /// point an `↑` / `↓` walk goes on from, with the draft parked one `↓` past
    /// the newest entry — so `↓` still finds the draft the search was opened
    /// over, or the one an earlier `↑` walk had parked. With nothing ever
    /// matched (an empty history, a query that never hit) the composer goes
    /// back to what it held: the search took it, and accepting nothing used to
    /// leave the composer empty and the draft gone.
    pub(crate) fn history_search_accept(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        match search.in_composer {
            Some(index) => {
                if search.walk.is_none() {
                    self.history_draft = search.draft;
                }
                self.history_cursor = Some(index);
            }
            None => {
                search.draft.restore(self);
                self.history_cursor = search.walk;
            }
        }
        self.refresh_completion();
    }

    /// Put the composer back as it was — the draft, or the prompt an `↑` walk
    /// had recalled, with the walk where it stood — and close the search.
    pub(crate) fn history_search_cancel(&mut self) {
        if let Some(search) = self.history_search.take() {
            search.draft.restore(self);
            self.history_cursor = search.walk;
            self.refresh_completion();
        }
    }

    /// Show the newest match strictly older than `before` (an index into
    /// `prompt_history`, `len` meaning "from the newest"), or say that there
    /// is none. Matching is case-insensitive on the prompt text, and each
    /// entry is lowercased once per refresh.
    fn history_search_refresh(&mut self, before: usize) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let query = search.query.to_lowercase();
        let matches: Vec<usize> = self
            .prompt_history
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.text.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        let found = matches.iter().rposition(|index| *index < before);
        let (status, short_count) = match found {
            Some(rank) => {
                let index = matches[rank];
                let recalled = self.prompt_history[index].clone();
                recalled.restore(self);
                let index = (matches.len() - rank).to_string();
                let total = matches.len().to_string();
                let args = [("index", index.as_str()), ("total", total.as_str())];
                (
                    self.tr_with(TextId::HistorySearchStatus, &args),
                    Some(self.tr_with(TextId::HistorySearchCountShort, &args)),
                )
            }
            None if self.prompt_history.is_empty() => {
                (self.tr(TextId::HistorySearchEmpty).into(), None)
            }
            None => (self.tr(TextId::HistorySearchNoMatch).into(), None),
        };
        if let Some(search) = self.history_search.as_mut() {
            search.shown = found.map(|rank| matches[rank]);
            if search.shown.is_some() {
                search.in_composer = search.shown;
            }
            search.status = status;
            search.short_count = short_count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::i18n::Lang;

    #[test]
    fn history_search_keeps_the_query_and_result_when_the_short_label_fits() {
        let mut app = App::new();
        app.lang = Lang::En;
        app.remember_prompt("earlier prompt");
        app.history_search_start();
        app.history_search_append("bogus text");

        // A 40-column terminal has 30 columns left after `[default] `.
        assert_eq!(
            app.history_search_display(30).unwrap(),
            ("Search '", "bogus text", "': no match")
        );
        assert_eq!(
            app.history_search_display(100).unwrap(),
            ("(reverse-i-search) '", "bogus text", "': no match"),
        );
    }

    #[test]
    fn history_search_keeps_the_no_match_result_when_clipping_the_query() {
        for (lang, available, matched, extra, expected) in [
            (
                Lang::En,
                30,
                "compile parser",
                'x',
                ("Search '", "…le parserx", "': no match"),
            ),
            (
                Lang::En,
                16,
                "compile parser",
                'x',
                ("?", "…serx", ": no match"),
            ),
            (
                Lang::Zh,
                23,
                "编译语法分析器错",
                '误',
                ("搜索 '", "…器错误", "'：无匹配"),
            ),
        ] {
            let mut app = App::new();
            app.lang = lang;
            app.remember_prompt(matched);
            app.history_search_start();
            app.history_search_append(matched);
            assert_eq!(app.input, matched);
            app.history_search_push(extra);

            assert_eq!(app.history_search_display(available).unwrap(), expected);
            assert_eq!(app.input, matched);
            assert_eq!(
                app.history_search_status().unwrap().1,
                format!("{matched}{extra}")
            );
        }
    }

    #[test]
    fn history_search_rechecks_fit_when_the_result_gains_a_digit() {
        let mut app = App::new();
        app.lang = Lang::En;
        for number in 1..=10 {
            app.remember_prompt(&format!("prompt {number} x"));
        }
        app.history_search_start();
        app.history_search_append("x");

        assert_eq!(
            app.history_search_display(37).unwrap(),
            ("(reverse-i-search) '", "x", "': match 1 of 10")
        );
        for _ in 1..10 {
            app.history_search_older();
        }
        assert_eq!(
            app.history_search_display(37).unwrap(),
            ("Search '", "x", "': match 10 of 10")
        );
    }

    #[test]
    fn history_search_results_always_fit_as_whole_fields() {
        for lang in [Lang::En, Lang::Zh] {
            let mut app = App::new();
            app.lang = lang;
            for number in 1..=10 {
                app.remember_prompt(&format!("prompt {number}"));
            }
            app.history_search_start();
            for rank in 1..=10 {
                let full = app.history_search_status().unwrap().2.to_string();
                let short = match lang {
                    Lang::En => format!("': {rank}/10"),
                    Lang::Zh => format!("'：{rank}/10"),
                };
                for width in 1..=40 {
                    let (label, query, result) = app.history_search_display(width).unwrap();
                    assert!(query.is_empty());
                    assert!(
                        label.width() + result.width().max(1) <= width,
                        "{width} columns: {label}{result}"
                    );
                    assert!([full.as_str(), short.as_str(), "'", ""].contains(&result));
                }
                app.history_search_older();
            }
        }
    }

    #[test]
    fn history_search_layout_tracks_available_width_and_query_edits() {
        let mut app = App::new();
        app.lang = Lang::En;
        app.remember_prompt("older needle");
        app.remember_prompt("newer needle");
        app.history_search_start();
        app.history_search_append("needle");

        let (_, query, suffix) = app.history_search_display(40).unwrap();
        assert_eq!(query, "needle");
        assert!(suffix.contains("match 1 of 2"));

        app.history_search_older();
        assert!(app.history_search.as_ref().unwrap().layout.is_some());
        let (_, query, suffix) = app.history_search_display(40).unwrap();
        assert_eq!(query, "needle");
        assert!(suffix.contains("match 2 of 2"));

        let (_, query, _) = app.history_search_display(6).unwrap();
        assert_eq!(query, "…dle");
        assert_eq!(app.history_search_display(40).unwrap().1, "needle");

        app.history_search_backspace();
        app.history_search_push('x');
        assert!(app.history_search.as_ref().unwrap().layout.is_none());
        let (_, query, suffix) = app.history_search_display(40).unwrap();
        assert_eq!(query, "needlx");
        assert!(suffix.contains("no match"));

        app.history_search_cancel();
        app.history_search_start();
        assert_eq!(app.history_search_display(40).unwrap().1, "");
    }
}
