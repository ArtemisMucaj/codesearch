use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::state::{AppState, SearchPane};
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;
use crate::tui::widgets::syntax;

use super::format::shorten_path;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    render_results(frame, panes[0], state);
    render_snippet(frame, panes[1], state);
}

fn render_results(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.search;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Results ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let entries: Vec<ListEntry> = s
        .results
        .iter()
        .map(|r| {
            let chunk = r.chunk();
            let label = format!("{}:{}", shorten_path(chunk.file_path()), chunk.start_line());
            let sub = chunk.symbol_name().map(|n| n.to_string());
            ListEntry {
                label,
                sub_label: sub,
                score: Some(r.score()),
            }
        })
        .collect();

    result_list::render(frame, area, "Results", &entries, s.selected);
}

fn render_snippet(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.search;

    // Suppress stale snippet when an error is active.
    let selected = if s.error.is_some() {
        None
    } else {
        s.results.get(s.selected)
    };

    // Border color reflects which pane has focus.
    let focused = s.focused_pane == SearchPane::Code;
    let border_color = if focused { Color::Cyan } else { Color::White };

    let (content, title, file_path, start_line) = match selected {
        Some(r) => (
            r.chunk().content().to_string(),
            shorten_path(r.chunk().file_path()),
            r.chunk().file_path().to_string(),
            r.chunk().start_line(),
        ),
        None => {
            let block = Block::default()
                .title("  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                Paragraph::new("  No snippet available.")
                    .block(block)
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
    };

    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = syntax::highlight_code(&content, &file_path, start_line as usize);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((s.snippet_scroll, 0));

    frame.render_widget(para, inner);
}
