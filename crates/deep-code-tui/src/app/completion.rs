use super::*;

impl App {
    #[must_use]
    pub(crate) fn completion_open(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn close_completion(&mut self) {
        self.completion = None;
    }

    /// Recompute the menu from the current input: `/command` prefix while no
    /// whitespace was typed, or a trailing `@file` token.
    pub(crate) fn refresh_completion(&mut self) {
        self.completion = self.compute_completion();
    }

    fn compute_completion(&self) -> Option<CompletionMenu> {
        if let Some(rest) = self.input.strip_prefix('/') {
            if self.input.contains(char::is_whitespace) {
                return None;
            }
            let filter = rest.to_lowercase();
            let items: Vec<(String, String)> = crate::commands::SLASH_COMMANDS
                .iter()
                .filter(|(name, _, _)| name[1..].starts_with(&filter))
                .map(|(name, hint, takes_arg)| {
                    let value = if *takes_arg {
                        format!("{name} ")
                    } else {
                        (*name).to_string()
                    };
                    (value, self.tr(*hint).to_string())
                })
                .collect();
            return (!items.is_empty()).then_some(CompletionMenu {
                kind: CompletionKind::Slash,
                items,
                selected: 0,
            });
        }

        let token_start = self.trailing_token_start();
        let token = &self.input[token_start..];
        let filter = token.strip_prefix('@')?;
        let filter_lower = filter.to_lowercase();
        let mut matched: Vec<&String> = self
            .workspace_files
            .iter()
            .filter(|file| file.to_lowercase().contains(&filter_lower))
            .collect();
        matched.sort_by_key(|file| {
            (
                !file.to_lowercase().starts_with(&filter_lower),
                file.len(),
                (*file).clone(),
            )
        });
        let items: Vec<(String, String)> = matched
            .into_iter()
            .take(COMPLETION_MENU_ITEMS)
            .map(|file| (file.clone(), String::new()))
            .collect();
        (!items.is_empty()).then_some(CompletionMenu {
            kind: CompletionKind::File,
            items,
            selected: 0,
        })
    }

    /// Byte index where the trailing whitespace-delimited token begins.
    fn trailing_token_start(&self) -> usize {
        self.input
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    }

    pub(crate) fn completion_up(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            let len = menu.items.len();
            menu.selected = (menu.selected + len - 1) % len;
        }
    }

    pub(crate) fn completion_down(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            menu.selected = (menu.selected + 1) % menu.items.len();
        }
    }

    /// Apply the selected completion to the input. Returns true when the
    /// completed value is a ready-to-run slash command (no argument), so the
    /// caller can submit immediately on Enter.
    pub(crate) fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some((value, _)) = menu.items.get(menu.selected) else {
            return false;
        };
        match menu.kind {
            CompletionKind::Slash => {
                self.input = value.clone();
                self.cursor_to_end();
                !value.ends_with(' ')
            }
            CompletionKind::File => {
                // Kept verbatim on purpose: this string is also what gets SENT,
                // and an `@`-reference has to name the file that actually
                // exists. The display side is handled where it belongs, by
                // `neutralize_composer_text` at render — a length-preserving
                // map, so `input_cursor` (a char index into this string) stays
                // valid.
                let token_start = self.trailing_token_start();
                self.input.truncate(token_start);
                self.input.push_str(value);
                self.input.push(' ');
                self.cursor_to_end();
                false
            }
        }
    }
}
