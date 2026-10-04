//! Composer editing operations.
//!
//! None of these check `is_streaming`: the composer stays fully live while a
//! turn streams, which is what makes mid-turn steering reachable. Typing,
//! editing, pasting and prompt-history recall all work mid-turn; `submit`
//! decides what to do with the text (queue it as a follow-up, or send it now)
//! — see `App::submit`. Re-adding an `is_streaming` early-return here silently
//! turns steering back into dead code, because the queue branch in `submit`
//! can only be reached with a non-empty composer.

use super::*;

impl App {
    pub fn push_char(&mut self, value: char) {
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert(byte, value);
        self.input_cursor = cursor + 1;
        self.history_cursor = None;
        self.refresh_completion();
    }

    pub fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        // A chip is one thing to the user, so it is one thing to Backspace.
        // Stepping into `[图片 #1 PNG]` a character at a time is never what
        // someone means, and it would leave a half-eaten chip for
        // `sync_images` to throw away — the picture vanishing with no gesture
        // that looks like it removed it.
        if let Some(index) = self.chip_ending_at_cursor() {
            let cursor = self.input_cursor.min(char_count(&self.input));
            let start = cursor.saturating_sub(char_count(&self.attached_images[index].chip));
            self.cut_chip(start, cursor);
            return;
        }
        let target = self.input_cursor.saturating_sub(1);
        if remove_char_at(&mut self.input, target) {
            self.input_cursor = target;
        }
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Insert a newline into the composer (Alt+Enter / Ctrl+J).
    pub fn push_newline(&mut self) {
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert(byte, '\n');
        self.input_cursor = cursor + 1;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete the character after the cursor (Delete key).
    pub fn delete_forward(&mut self) {
        if self.input_cursor >= char_count(&self.input) {
            return;
        }
        // Mirror of Backspace: a chip in front of the cursor goes as a unit.
        if let Some(index) = self.chip_starting_at_cursor() {
            let start = self.input_cursor;
            let end = start + char_count(&self.attached_images[index].chip);
            self.cut_chip(start, end);
            return;
        }
        remove_char_at(&mut self.input, self.input_cursor);
        // cursor stays — next char slides left into its place.
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Remove one attachment chip as a unit.
    ///
    /// `start`/`end` are char indices into `input`: Backspace deletes a chip that
    /// ends at the cursor, Delete deletes one that starts there, and both land
    /// here so the two paths cannot drift on the other bookkeeping.
    ///
    /// The attachment itself is *not* removed here — `sync_images`, reached
    /// through `refresh_completion`, drops it only once no copy of its chip is
    /// left in the draft. That is what makes a chip the user copied and pasted a
    /// second time behave: deleting one copy keeps the image, because the other
    /// copy is still addressing it, and deleting the last one takes it away.
    fn cut_chip(&mut self, start: usize, end: usize) {
        self.drain_chars(start, end);
        self.input_cursor = start;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Move cursor one character left.
    pub fn cursor_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.input_cursor -= 1;
    }

    /// Move cursor one character right.
    pub fn cursor_right(&mut self) {
        if self.input_cursor < char_count(&self.input) {
            self.input_cursor += 1;
        }
    }

    /// Move cursor to start of the current logical line.
    pub fn cursor_home(&mut self) {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let prefix = &self.input[..byte];
        let line_start_byte = prefix.rfind('\n').map_or(0, |pos| pos + 1);
        self.input_cursor = self.input[..line_start_byte].chars().count();
    }

    /// Move cursor to end of the current logical line.
    pub fn cursor_end(&mut self) {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let tail = &self.input[byte..];
        let eol = tail.find('\n').unwrap_or(tail.len());
        let line_end_byte = byte + eol;
        self.input_cursor = self.input[..line_end_byte].chars().count();
    }

    /// Move cursor to the very end of input.
    pub fn cursor_to_end(&mut self) {
        self.input_cursor = char_count(&self.input);
    }

    pub(crate) fn insert_str_at_cursor(&mut self, text: &str) {
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert_str(byte, text);
        self.input_cursor = cursor + char_count(text);
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Handle a bracketed-paste payload. Large pastes (multi-line or long)
    /// collapse to a compact `[粘贴 #N …]` chip whose real content is kept in
    /// `pasted_blocks` and expanded back in on submit; short single-line
    /// pastes insert inline.
    pub fn paste_str(&mut self, text: String) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            // An empty paste is a signal, not a no-op. A pasteboard carrying
            // pixels and no text — a screenshot — leaves some terminals with
            // nothing to send, and those that send the empty paste anyway are
            // telling us a paste was requested. Nothing else an empty paste could
            // mean, and the clipboard read is the only way to find out.
            self.attach_clipboard_image_quietly();
            return;
        }
        // A drag-and-drop arrives as the file's path, so this is the only thing
        // between "drag an image in" and reading a `/Users/…` path back off your
        // own screen. All-or-nothing: a paste that merely mentions a path is
        // text, which is what `@` and `/image` are for. The clipboard fallback
        // for a bare *name* a terminal handed over lives in there too.
        if self.attach_pasted_images(&normalized) {
            return;
        }
        let multiline = normalized.contains('\n');
        let chars = char_count(&normalized);
        if multiline || chars > 120 {
            let id = self.pasted_blocks.len() + 1;
            // The rendered chip text is also the expansion token stored in
            // `pasted_blocks`, so a mid-draft /lang switch cannot orphan it.
            let placeholder = if multiline {
                self.tr_with(
                    TextId::PasteChipLines,
                    &[
                        ("id", &id.to_string()),
                        ("lines", &normalized.lines().count().max(1).to_string()),
                    ],
                )
            } else {
                self.tr_with(
                    TextId::PasteChipChars,
                    &[("id", &id.to_string()), ("chars", &chars.to_string())],
                )
            };
            self.insert_str_at_cursor(&placeholder);
            self.pasted_blocks.push((placeholder, normalized));
        } else {
            self.insert_str_at_cursor(&normalized);
        }
    }

    /// Replace any collapsed-paste placeholders with their real content.
    pub(crate) fn expand_pasted(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, content) in &self.pasted_blocks {
            out = out.replace(placeholder.as_str(), content);
        }
        out
    }

    /// Delete the word (and any whitespace) before the cursor (Ctrl+W).
    pub fn delete_word_back(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.input_cursor.min(chars.len());
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.drain_chars(start, self.input_cursor);
        self.input_cursor = start;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete from the current logical line's start up to the cursor (Ctrl+U).
    pub fn kill_to_line_start(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let start = self.current_line_start_char();
        self.drain_chars(start, self.input_cursor);
        self.input_cursor = start;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete from the cursor to the end of the current logical line (Ctrl+K).
    pub fn kill_to_line_end(&mut self) {
        let end = self.current_line_end_char();
        self.drain_chars(self.input_cursor, end);
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Move cursor to the previous word start (Ctrl/Alt + Left).
    pub fn word_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.input_cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.input_cursor = i;
    }

    /// Move cursor to the next word end (Ctrl/Alt + Right).
    pub fn word_right(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let len = chars.len();
        let mut i = self.input_cursor.min(len);
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        self.input_cursor = i;
    }

    pub(crate) fn drain_chars(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let mut chars: Vec<char> = self.input.chars().collect();
        let end = end.min(chars.len());
        chars.drain(start..end);
        self.input = chars.into_iter().collect();
    }

    fn current_line_start_char(&self) -> usize {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let start_byte = self.input[..byte].rfind('\n').map_or(0, |pos| pos + 1);
        self.input[..start_byte].chars().count()
    }

    fn current_line_end_char(&self) -> usize {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let eol = self.input[byte..]
            .find('\n')
            .unwrap_or(self.input.len() - byte);
        self.input[..byte + eol].chars().count()
    }

    /// Up arrow drives the composer only — never the transcript (that's
    /// PageUp/PageDown). In a multi-line draft it moves the cursor up a line
    /// (no-op at the top, so the draft is never clobbered); on a single-line
    /// or empty composer it recalls the previous prompt.
    pub fn on_up(&mut self) {
        if self.input.contains('\n') {
            self.cursor_up_logical();
        } else {
            self.history_prev();
        }
    }

    /// Down arrow: mirror of [`Self::on_up`].
    pub fn on_down(&mut self) {
        if self.input.contains('\n') {
            self.cursor_down_logical();
        } else {
            self.history_next();
        }
    }

    /// Move one logical line up, preserving column. Returns false when already
    /// on the first line (so the caller can fall back to history).
    fn cursor_up_logical(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return false;
        }
        let target = line - 1;
        let len = self.logical_line_len(target);
        self.input_cursor = self.line_start_char(target) + col.min(len);
        true
    }

    fn cursor_down_logical(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        let total = self.input.split('\n').count();
        if line + 1 >= total {
            return false;
        }
        let target = line + 1;
        let len = self.logical_line_len(target);
        self.input_cursor = self.line_start_char(target) + col.min(len);
        true
    }

    /// (logical line index, column in chars) of the cursor.
    pub(crate) fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.input.chars().enumerate() {
            if i >= self.input_cursor {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Char index where logical line `line` starts.
    fn line_start_char(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0usize;
        for (i, ch) in self.input.chars().enumerate() {
            if ch == '\n' {
                seen += 1;
                if seen == line {
                    return i + 1;
                }
            }
        }
        char_count(&self.input)
    }

    fn logical_line_len(&self, line: usize) -> usize {
        self.input
            .split('\n')
            .nth(line)
            .map_or(0, |l| l.chars().count())
    }

    /// Recall the previous sent prompt (Ctrl+P). The live input is stashed as
    /// a draft and restored when navigating past the newest entry.
    pub fn history_prev(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let cursor = match self.history_cursor {
            None => {
                // Park the in-progress draft, images and all, so `↓` puts back
                // what the user was actually writing.
                self.history_draft = HistoryPrompt {
                    text: std::mem::take(&mut self.input),
                    images: std::mem::take(&mut self.attached_images),
                };
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_cursor = Some(cursor);
        let recalled = self.prompt_history[cursor].clone();
        recalled.restore(self);
    }

    /// Walk back toward the draft (Ctrl+N).
    pub fn history_next(&mut self) {
        match self.history_cursor {
            None => {}
            Some(index) if index + 1 < self.prompt_history.len() => {
                self.history_cursor = Some(index + 1);
                let recalled = self.prompt_history[index + 1].clone();
                recalled.restore(self);
            }
            Some(_) => {
                self.history_cursor = None;
                let draft = std::mem::take(&mut self.history_draft);
                draft.restore(self);
            }
        }
    }

    pub(crate) fn remember_prompt(&mut self, prompt: &str) {
        if self.prompt_history.last().map(|entry| entry.text.as_str()) != Some(prompt) {
            // The attachments ride along, so recalling this prompt restores its
            // pictures rather than text that talks about a `[图片 #1 PNG]` the
            // composer no longer has.
            self.prompt_history.push(HistoryPrompt {
                text: prompt.to_string(),
                images: self.attached_images.clone(),
            });
            if self.prompt_history.len() > PROMPT_HISTORY_CAP {
                self.prompt_history.remove(0);
            }
        }
        self.history_cursor = None;
        self.history_draft = HistoryPrompt::default();
    }
}
