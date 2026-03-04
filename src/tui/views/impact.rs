use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::application::ImpactNode;
use crate::tui::state::AppState;
use crate::tui::widgets::{flame_graph, result_list};
use crate::tui::widgets::result_list::ListEntry;

use super::format::{short_symbol, shorten_path};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes = Layout::horizontal([
        Constraint::Percentage(35),
        Constraint::Percentage(65),
    ])
    .split(area);

    render_affected(frame, panes[0], state);
    render_flame(frame, panes[1], state);
}

fn render_affected(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.impact;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Affected ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err)).block(block),
            area,
        );
        return;
    }

    let entries: Vec<ListEntry> = match &s.analysis {
        None => vec![],
        Some(analysis) => flat_nodes(&analysis.by_depth)
            .iter()
            .map(|node| ListEntry {
                label: format!(
                    "{}:{}",
                    shorten_path(&node.file_path),
                    node.line
                ),
                sub_label: Some(format!(
                    "depth {} · {}",
                    node.depth,
                    short_symbol(&node.symbol)
                )),
                score: None,
            })
            .collect(),
    };

    result_list::render(frame, area, "Affected", &entries, s.selected);
}

fn render_flame(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.impact;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Blast Radius ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err)).block(block),
            area,
        );
        return;
    }

    match &s.analysis {
        None => {
            let block = Block::default()
                .title(" Blast Radius ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            let hint = if s.loading {
                "  Analysing…"
            } else {
                "  Enter a symbol and press Enter."
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .block(block)
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
        }
        Some(analysis) => {
            flame_graph::render(frame, area, analysis, s.flame_scroll);
        }
    }
}

/// Flatten `by_depth` into a single ordered list for the left-pane list widget.
fn flat_nodes(by_depth: &[Vec<ImpactNode>]) -> Vec<&ImpactNode> {
    by_depth.iter().flatten().collect()
}

