//! Ctrl+R: reverse search through the prompt history, readline style.
//!
//! While the search is open the composer shows the newest prompt that
//! contains the query, and the status line shows the query and where the
//! match sits. Typing narrows, Ctrl+R (or ↑) steps to the next older match,
//! Enter keeps the shown prompt in the composer for editing or sending, Esc
//! puts the draft back exactly as it was. Any other key keeps the match and
//! goes on to do its usual job, so `Ctrl+R fix Ctrl+A` lands at the start of
//! the recalled prompt.
//!
//! Accepting does not send: a recalled prompt re-runs an agent, which is a
//! heavier thing than re-running a shell command, so it gets a look first.

use super::{App, HistoryPrompt};
use deep_code_agent::i18n::TextId;

/// The open search: what the user typed, the draft that was in the composer
/// when it opened, and which history entry is currently shown.
#[derive(Debug, Clone)]
pub(crate) struct HistorySearch {
    query: String,
    draft: HistoryPrompt,
    /// Index into `prompt_history` of the shown match; `None` while nothing
    /// matches (the composer then keeps the last match, like readline).
    shown: Option<usize>,
}

impl App {
    #[must_use]
    pub(crate) fn history_search_open(&self) -> bool {
        self.history_search.is_some()
    }

    /// Ctrl+R: open the search over the current draft, or step to the next
    /// older match when it is already open.
    pub(crate) fn history_search_start(&mut self) {
        if self.history_search.is_some() {
            self.history_search_older();
            return;
        }
        self.close_completion();
        self.history_cursor = None;
        self.history_search = Some(HistorySearch {
            query: String::new(),
            draft: HistoryPrompt {
                text: std::mem::take(&mut self.input),
                images: std::mem::take(&mut self.attached_images),
            },
            shown: None,
        });
        self.input_cursor = 0;
        self.history_search_refresh(self.prompt_history.len());
    }

    pub(crate) fn history_search_push(&mut self, ch: char) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        search.query.push(ch);
        // A narrower query is searched from the newest entry again: the match
        // the broader one landed on may no longer qualify, and the newest
        // qualifying one is the readline answer.
        self.history_search_refresh(self.prompt_history.len());
    }

    pub(crate) fn history_search_backspace(&mut self) {
        let Some(search) = self.history_search.as_mut() else {
            return;
        };
        search.query.pop();
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

    /// Keep what the composer shows and close the search.
    pub(crate) fn history_search_accept(&mut self) {
        if self.history_search.take().is_some() {
            self.status.clear();
            self.refresh_completion();
        }
    }

    /// Put the draft back and close the search.
    pub(crate) fn history_search_cancel(&mut self) {
        if let Some(search) = self.history_search.take() {
            search.draft.restore(self);
            self.status.clear();
            self.refresh_completion();
        }
    }

    /// Show the newest match strictly older than `before` (an index into
    /// `prompt_history`, `len` meaning "from the newest"), or say that there
    /// is none. Matching is case-insensitive on the prompt text.
    fn history_search_refresh(&mut self, before: usize) {
        let Some(search) = self.history_search.as_ref() else {
            return;
        };
        let query = search.query.to_lowercase();
        let found = self.prompt_history[..before.min(self.prompt_history.len())]
            .iter()
            .rposition(|entry| entry.text.to_lowercase().contains(&query));
        let total = self
            .prompt_history
            .iter()
            .filter(|entry| entry.text.to_lowercase().contains(&query))
            .count();
        let query_shown = search.query.clone();
        match found {
            Some(index) => {
                let recalled = self.prompt_history[index].clone();
                recalled.restore(self);
                let position = self.prompt_history[index..]
                    .iter()
                    .filter(|entry| entry.text.to_lowercase().contains(&query))
                    .count();
                if let Some(search) = self.history_search.as_mut() {
                    search.shown = Some(index);
                }
                self.status = self.tr_with(
                    TextId::HistorySearchStatus,
                    &[
                        ("query", &query_shown),
                        ("index", &position.to_string()),
                        ("total", &total.to_string()),
                    ],
                );
            }
            None => {
                if let Some(search) = self.history_search.as_mut() {
                    search.shown = None;
                }
                self.status = if self.prompt_history.is_empty() {
                    self.tr(TextId::HistorySearchEmpty).to_string()
                } else {
                    self.tr_with(TextId::HistorySearchNoMatch, &[("query", &query_shown)])
                };
            }
        }
    }
}
