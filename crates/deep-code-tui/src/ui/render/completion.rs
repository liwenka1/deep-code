//! The completion menu: a windowed list over slash-command and `@`-file
//! candidates, drawn above the composer.

use super::*;
use crate::app::CompletionKind;

/// Max completion rows shown at once; the list windows around the selection so
/// wrapping past the top/bottom keeps the highlighted item on screen.
pub(super) const COMPLETION_VISIBLE_ROWS: usize = 8;

pub(super) fn render_completion_menu(
    frame: &mut Frame<'_>,
    menu: &crate::app::CompletionMenu,
    area: ratatui::layout::Rect,
    lang: Lang,
) {
    if area.is_empty() {
        return;
    }
    let title = match menu.kind {
        CompletionKind::ModelPicker => tr(lang, TextId::ModelPickerModel),
        CompletionKind::ReasoningPicker => tr(lang, TextId::ModelPickerReasoning),
        CompletionKind::Slash | CompletionKind::File => tr(lang, TextId::CompletionMenuTitle),
    };
    let block = Block::default().border_style(Style::default().fg(Color::Blue));
    let block = match area.height {
        1 => block,
        2 => block.title(title).borders(Borders::TOP),
        _ => block.title(title).borders(Borders::ALL),
    };
    let visible_rows = usize::from(block.inner(area).height).min(COMPLETION_VISIBLE_ROWS);
    // Window around the selection so wrapping to the last/first item keeps the
    // highlight visible instead of scrolling it off the top of the list.
    let start = menu.selected.saturating_sub(visible_rows.saturating_sub(1));
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, (value, hint))| {
            let marker = if index == menu.selected { "▶ " } else { "  " };
            let mut spans = vec![Span::raw(marker.to_string())];
            // `value` is a workspace FILENAME (`@` completion reads
            // `list_workspace_files`), so it is whatever is on disk — a cloned
            // repo or a model write can name a file `evil\x1b[8mhidden.txt`.
            // `Paragraph` filters zero-width symbols but not `\x1b`, which
            // measures 1, so an unsanitized menu row flushes the escape to the
            // terminal; SGR conceal turned on here survives into later frames,
            // including an approval panel. Nothing else guards this path.
            let value_span = Span::raw(neutralize_display_text(value));
            if index == menu.selected {
                spans.push(value_span.bold());
            } else {
                spans.push(value_span);
            }
            if !hint.is_empty() {
                // Sanitized for the same reason as `value` above, not because
                // today's hints are hostile: they are i18n for slash items and
                // empty for file items. An unfiltered span sitting beside a
                // filtered one is how the last several of these got in — the
                // asymmetry reads as a deliberate exemption.
                spans.push(Span::styled(
                    format!("  {}", neutralize_display_text(hint)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
