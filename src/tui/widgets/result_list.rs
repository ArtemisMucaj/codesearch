use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

/// A list entry with a display label and an optional score badge.
pub struct ListEntry {
    pub label: String,
    pub sub_label: Option<String>,
    pub score: Option<f32>,
}

/// Render a scrollable list of entries with selection highlighting.
pub fn render(frame: &mut Frame, area: Rect, title: &str, entries: &[ListEntry], selected: usize) {
    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let is_selected = i == selected;
            let bullet = if is_selected { "●" } else { "○" };
            let score_badge = e.score.map(|s| format!("  {:.2}", s)).unwrap_or_default();

            let label_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let score_style = Style::default().fg(Color::DarkGray);

            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{} ", bullet), label_style),
                Span::styled(e.label.clone(), label_style),
                Span::styled(score_badge, score_style),
            ])];

            if let Some(sub) = &e.sub_label {
                lines.push(Line::from(Span::styled(
                    format!("   {}", sub),
                    Style::default().fg(Color::DarkGray),
                )));
            }

            ListItem::new(lines)
        })
        .collect();

    let border_style = if entries.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" {} ({}) ", title, entries.len()))
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default();
    if !entries.is_empty() {
        list_state.select(Some(selected));
    }

    frame.render_stateful_widget(list, area, &mut list_state);
}
