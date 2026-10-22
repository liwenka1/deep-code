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
    /// The status row's text for the search. Kept here, not in `App::status`,
    /// which a streaming turn rewrites on every tool call: the search has to
    /// stay visible however long it is open.
    status: String,
}

impl App {
    #[must_use]
    pub(crate) fn history_search_open(&self) -> bool {
        self.history_search.is_some()
    }

    /// What the status row shows while the search is open.
    #[must_use]
    pub(crate) fn history_search_status(&self) -> Option<&str> {
        self.history_search
            .as_ref()
            .map(|search| search.status.as_str())
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
        let status = match found {
            Some(rank) => {
                let index = matches[rank];
                let recalled = self.prompt_history[index].clone();
                recalled.restore(self);
                self.tr_with(
                    TextId::HistorySearchStatus,
                    &[
                        ("query", &query_shown(self)),
                        ("index", &(matches.len() - rank).to_string()),
                        ("total", &matches.len().to_string()),
                    ],
                )
            }
            None if self.prompt_history.is_empty() => {
                self.tr(TextId::HistorySearchEmpty).to_string()
            }
            None => self.tr_with(
                TextId::HistorySearchNoMatch,
                &[("query", &query_shown(self))],
            ),
        };
        if let Some(search) = self.history_search.as_mut() {
            search.shown = found.map(|rank| matches[rank]);
            if search.shown.is_some() {
                search.in_composer = search.shown;
            }
            search.status = status;
        }
    }
}

/// The query as typed, for the status row.
fn query_shown(app: &App) -> String {
    app.history_search
        .as_ref()
        .map(|search| search.query.clone())
        .unwrap_or_default()
}
