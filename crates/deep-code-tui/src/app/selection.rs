use super::*;

impl App {
    /// Record what the transcript render produced, for mouse → text mapping.
    pub(crate) fn set_transcript_snapshot(&mut self, snap: TranscriptSnapshot) {
        self.transcript = Some(snap);
    }

    /// `/find`: jump to the nearest earlier transcript line containing
    /// `query` (case-insensitive). Repeating the same query continues upward;
    /// exhausting matches resets so the next `/find` starts from the bottom.
    /// Searches the last render's plain-text lines, so what it finds is
    /// exactly what is on screen.
    pub(crate) fn find_in_transcript(&mut self, query: &str) {
        let Some(snapshot) = self.transcript.as_ref() else {
            self.status = self.tr(TextId::FindNoTranscript).to_string();
            return;
        };
        let needle = query.to_lowercase();
        let total = snapshot.lines.len();
        let search_end = match &self.find_state {
            Some((previous, index)) if previous == query => (*index).min(total),
            _ => total,
        };
        let matched = snapshot.lines[..search_end]
            .iter()
            .rposition(|line| line.to_lowercase().contains(&needle));
        match matched {
            Some(line_index) => {
                let viewport = usize::from(snapshot.height).max(1);
                let max_scroll = total.saturating_sub(viewport);
                // scroll_offset counts lines up from the bottom; putting the
                // match at the top of the viewport means scroll_top ==
                // line_index (clamped to the scrollable range).
                self.scroll_offset = max_scroll.saturating_sub(line_index);
                self.find_state = Some((query.to_string(), line_index));
                self.status = self.tr_with(
                    TextId::FindFound,
                    &[("query", query), ("line", &(line_index + 1).to_string())],
                );
            }
            None => {
                let was_continuing = self
                    .find_state
                    .take()
                    .is_some_and(|(previous, _)| previous == query);
                if was_continuing {
                    self.status = self.tr_with(TextId::FindExhausted, &[("query", query)]);
                } else {
                    self.status = self.tr_with(TextId::FindNotFound, &[("query", query)]);
                }
            }
        }
    }

    /// Map an absolute mouse `(col, row)` to a `(line, display_col)` position
    /// in the transcript buffer, or `None` if outside the transcript area.
    fn mouse_to_text(&self, col: u16, row: u16) -> Option<TextPos> {
        let snap = self.transcript.as_ref()?;
        if row < snap.y
            || row >= snap.y.saturating_add(snap.height)
            || col < snap.x
            || col >= snap.x.saturating_add(snap.width)
        {
            return None;
        }
        // Text starts one column in (the left padding gutter).
        let text_x = snap.x.saturating_add(1);
        let line = snap.scroll_top + usize::from(row - snap.y);
        if line >= snap.lines.len() {
            // Below the last line → clamp to end of the last line.
            let last = snap.lines.len().saturating_sub(1);
            let width = snap.lines.get(last).map_or(0, |l| display_width(l));
            return Some((last, width));
        }
        let display_col = usize::from(col.saturating_sub(text_x));
        let max = display_width(&snap.lines[line]);
        Some((line, display_col.min(max)))
    }

    /// Begin a selection at a mouse position (left button down).
    pub(crate) fn selection_begin(&mut self, col: u16, row: u16) {
        match self.mouse_to_text(col, row) {
            Some(pos) => self.selection = Some((pos, pos)),
            None => self.selection = None,
        }
    }

    /// Extend the in-progress selection (left button drag).
    pub(crate) fn selection_update(&mut self, col: u16, row: u16) {
        if let (Some((anchor, _)), Some(pos)) = (self.selection, self.mouse_to_text(col, row)) {
            self.selection = Some((anchor, pos));
        }
    }

    /// Finish a selection (left button up): returns the selected text to copy,
    /// or `None` for an empty selection (a plain click), which clears it.
    pub(crate) fn selection_finish(&mut self) -> Option<String> {
        let (anchor, head) = self.selection?;
        if anchor == head {
            self.selection = None;
            return None;
        }
        self.selected_text()
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Extract the currently selected transcript text.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let snap = self.transcript.as_ref()?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        for line in start.0..=end.0 {
            let text = snap.lines.get(line)?;
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if line == end.0 {
                end.1
            } else {
                display_width(text)
            };
            out.push_str(&slice_by_display_cols(text, from, to));
            if line != end.0 {
                out.push('\n');
            }
        }
        Some(out)
    }
}
