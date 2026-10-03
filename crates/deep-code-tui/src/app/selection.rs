use super::*;
use crate::app::ClickPress;

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

    /// Left button down: start a selection and remember what the press landed
    /// on, so the release can tell a click from a drag.
    pub(crate) fn mouse_press(&mut self, col: u16, row: u16) {
        let target = self.fold_target_at(col, row);
        self.mouse_down = Some(ClickPress {
            col,
            row,
            target,
            trimmed_cells: self.trimmed_cells,
        });
        self.selection_begin(col, row);
    }

    /// Left button up: the dragged text to copy, or `None` when nothing was
    /// dragged — in which case a press/release pair on a foldable header folds
    /// or unfolds that block.
    pub(crate) fn mouse_release(&mut self, col: u16, row: u16) -> Option<String> {
        let click = self.take_click(col, row);
        let copied = self.selection_finish();
        if copied.is_none()
            && let Some(target) = click
        {
            self.toggle_fold(target);
        }
        copied
    }

    /// Consume the pending press. `Some(target)` only when the release landed in
    /// the same cell the press did — a drag that started on a header must select
    /// text, never fold the block.
    fn take_click(&mut self, col: u16, row: u16) -> Option<FoldTarget> {
        let press = self.mouse_down.take()?;
        if (press.col, press.row) != (col, row) {
            return None;
        }
        // `enforce_history_cap` drains from the FRONT, so a history index
        // recorded before such a drain now names a cell that many places
        // earlier. Without this correction the click would fold whichever block
        // happens to sit at the stale index — same cell variant, wrong block.
        // (`checked_sub` answers `None` when the pressed cell was itself one of
        // the dropped ones, which is the honest outcome.)
        let dropped = self.trimmed_cells.saturating_sub(press.trimmed_cells);
        match press.target? {
            FoldTarget::HistoryReasoning(index) => {
                index.checked_sub(dropped).map(FoldTarget::HistoryReasoning)
            }
            FoldTarget::HistoryToolBatch(index) => {
                index.checked_sub(dropped).map(FoldTarget::HistoryToolBatch)
            }
            live @ FoldTarget::LiveReasoning => Some(live),
        }
    }

    /// What the foldable header at an absolute mouse position controls, if the
    /// position is on one. Resolved against the last render's snapshot, which is
    /// exactly what the user is looking at.
    fn fold_target_at(&self, col: u16, row: u16) -> Option<FoldTarget> {
        let snapshot = self.transcript.as_ref()?;
        let (line, _) = self.mouse_to_text(col, row)?;
        snapshot
            .fold_headers
            .iter()
            .find(|(at, _)| *at == line)
            .map(|(_, target)| *target)
    }

    /// Fold or unfold one block, wherever it lives.
    fn toggle_fold(&mut self, target: FoldTarget) {
        match target {
            FoldTarget::HistoryReasoning(index) => {
                if let Some(HistoryCell::Reasoning { expanded, .. }) = self.history.get_mut(index) {
                    *expanded = !*expanded;
                }
            }
            FoldTarget::HistoryToolBatch(index) => {
                if let Some(HistoryCell::ToolBatch { expanded, .. }) = self.history.get_mut(index) {
                    *expanded = !*expanded;
                }
            }
            FoldTarget::LiveReasoning => {
                if let Some(active) = self.active_turn.as_mut() {
                    active.reasoning_expanded = !active.reasoning_expanded;
                }
            }
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

    /// Forget the pointer's whole gesture over the transcript.
    ///
    /// Both halves are coordinates into the transcript a caller replaces when it
    /// calls this: the selection, and the press that may still be waiting for its
    /// release. Dropping only the first leaves a release to be matched against
    /// the same index in a *different* history — `/clear` and `/resume` swap the
    /// transcript wholesale without touching the cap counter the release uses to
    /// correct for a trim, so the index would name whatever block now sits there.
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
        self.mouse_down = None;
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
