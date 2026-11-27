use super::*;
use crate::app::RestorePicker;

pub(super) fn render_restore_picker(frame: &mut Frame<'_>, picker: &RestorePicker, lang: Lang) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let [title, list, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(2),
    ])
    .areas(area);
    let padded = || Block::default().padding(Padding::new(1, 0, 0, 0));
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Span::styled(tr(lang, TextId::CheckpointsHeader), accent)).block(padded()),
        title,
    );

    let viewport = usize::from(list.height);
    let start = picker.selected.saturating_sub(viewport.saturating_sub(1));
    let rows: Vec<_> = picker
        .rounds
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, round)| {
            let selected = index == picker.selected;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected { accent } else { Style::default() };
            let number = round.round.to_string();
            let label = match round
                .checkpoints
                .iter()
                .min_by_key(|point| point.before_turn)
            {
                Some(point) if point.before_turn => TextId::RestoreBeforeTurn,
                Some(_) => TextId::RestoreAfterTurn,
                None => TextId::RestoreNoSnapshot,
            };
            let label = tr_with(lang, label, &[("round", &number)]);
            let prompt = round
                .prompt
                .as_deref()
                .unwrap_or(tr(lang, TextId::RestorePromptMissing));
            let images = if round.image_count > 0 {
                tr_with(
                    lang,
                    TextId::RestoreImageCount,
                    &[("count", &round.image_count.to_string())],
                )
            } else {
                String::new()
            };
            Line::from(vec![
                Span::styled(format!("{prefix}{label}  "), style),
                Span::styled(images, Style::default().fg(Color::DarkGray)),
                Span::raw(neutralize_display_text(prompt)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows).block(padded()), list);

    let ids = picker.rounds[picker.selected]
        .checkpoints
        .iter()
        .map(|point| point.id.0.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(tr(lang, TextId::UsageRestore)),
            Line::from(neutralize_display_text(&ids)),
        ])
        .style(Style::default().fg(Color::DarkGray))
        .block(padded()),
        help,
    );
}
