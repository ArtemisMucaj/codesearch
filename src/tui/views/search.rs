use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::state::AppState;
use crate::tui::widgets::{code_panel, result_list};
use crate::tui::widgets::result_list::ListEntry;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes = Layout::horizontal([
        Constraint::Percentage(35),
        Constraint::Percentage(65),
    ])
    .split(area);

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
            Paragraph::new(format!("Error: {}", err)).block(block),
            area,
        );
        return;
    }

    let entries: Vec<ListEntry> = s
        .results
        .iter()
        .map(|r| {
            let chunk = r.chunk();
            let label = format!(
                "{}:{}",
                shorten_path(chunk.file_path()),
                chunk.start_line()
            );
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
    let selected = s.results.get(s.selected);

    let (content, title, start_line) = match selected {
        Some(r) => (
            r.chunk().content().to_string(),
            shorten_path(r.chunk().file_path()),
            r.chunk().start_line(),
        ),
        None => (String::new(), String::new(), 0),
    };

    code_panel::render(frame, area, &title, &content, start_line, s.snippet_scroll);
}

fn shorten_path(path: &str) -> String {
    // Show at most the last two path components for readability.
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
}
