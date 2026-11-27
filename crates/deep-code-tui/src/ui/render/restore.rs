use super::*;
use crate::app::{RestorePicker, RestoreReader};
use deep_code_agent::{EntryKind, SessionEntry};

pub(super) fn render_restore_picker(
    frame: &mut Frame<'_>,
    picker: &RestorePicker,
    status: &str,
    lang: Lang,
) {
    let ids = picker.rounds[picker.selected]
        .checkpoints
        .iter()
        .map(|point| point.id.0.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    let detail = if status.is_empty() { &ids } else { status };
    let list = render_restore_panel(
        frame,
        tr(lang, TextId::CheckpointsHeader),
        vec![
            Line::from(tr(lang, TextId::UsageRestore)),
            Line::from(neutralize_display_text(detail)),
        ],
    );
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

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
}

pub(super) fn render_restore_reader(frame: &mut Frame<'_>, reader: &mut RestoreReader, lang: Lang) {
    let title = tr_with(
        lang,
        TextId::RestoreRoundTitle,
        &[
            ("round", &reader.round.to_string()),
            ("entry", &(reader.selected + 1).to_string()),
            ("total", &reader.entries.len().to_string()),
        ],
    );
    let body = render_restore_panel(
        frame,
        &title,
        vec![Line::from(tr(lang, TextId::RestoreRoundControls))],
    );
    let width = body.width.saturating_sub(1).max(1);
    if reader
        .lines
        .as_ref()
        .is_none_or(|(previous, _)| *previous != width)
    {
        let entry = &reader.entries[reader.selected];
        reader.lines = Some((width, original_entry_lines(entry, width, lang)));
    }
    let (_, lines) = reader
        .lines
        .as_ref()
        .expect("selected entry has been rendered");
    reader.height = usize::from(body.height);
    reader.scroll = reader.scroll.min(lines.len().saturating_sub(reader.height));
    let visible: Vec<_> = lines
        .iter()
        .skip(reader.scroll)
        .take(reader.height)
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(visible).block(padded()), body);
}

fn original_entry_lines(entry: &SessionEntry, width: u16, lang: Lang) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut push = |cell: HistoryCell| lines.extend(cell_lines(&cell, width, lang));
    match &entry.kind {
        EntryKind::User { content } => push(HistoryCell::user(content.summary_text())),
        EntryKind::System { content } => push(HistoryCell::system(content.clone())),
        EntryKind::Assistant {
            content,
            reasoning,
            exchanges,
        } => {
            if let Some(text) = reasoning {
                push(HistoryCell::Reasoning {
                    text: text.clone(),
                    expanded: true,
                });
            }
            if !content.is_empty() {
                push(HistoryCell::assistant(content.clone()));
            }
            for exchange in exchanges {
                push(HistoryCell::system(format!(
                    "{} ({})\n{}",
                    exchange.call.function.name, exchange.call.id, exchange.call.function.arguments,
                )));
                if let Some(result) = &exchange.result {
                    push(HistoryCell::ToolResult {
                        status: result.status,
                        summary: result.content.summary_text(),
                    });
                }
            }
        }
        EntryKind::Compaction { .. } => {}
    }
    lines
}

fn render_restore_panel(frame: &mut Frame<'_>, heading: &str, footer: Vec<Line<'_>>) -> Rect {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let [title, body, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(footer.len() as u16),
    ])
    .areas(area);
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Span::styled(heading, accent)).block(padded()),
        title,
    );
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(Color::DarkGray))
            .block(padded()),
        help,
    );
    body
}

fn padded() -> Block<'static> {
    Block::default().padding(Padding::new(1, 0, 0, 0))
}
