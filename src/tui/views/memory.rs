use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::MemoryEntry;
use crate::tui::state::{AppState, MemoryPane};
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);

    render_list(frame, panes[0], state);
    render_detail(frame, panes[1], state);
}

fn render_list(frame: &mut Frame, area: Rect, state: &AppState) {
    let m = &state.memory;

    if let Some(err) = &m.error {
        let block = Block::default()
            .title(" Memory ")
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

    // Score badge only carries meaning for a query (browse mode scores are 0).
    let show_scores = !m.input.trim().is_empty();
    let entries: Vec<ListEntry> = m
        .entries
        .iter()
        .map(|e| ListEntry {
            label: format!("[{}] {}", e.kind_label(), e.label()),
            sub_label: entry_preview(e),
            score: if show_scores { Some(e.score()) } else { None },
        })
        .collect();

    let title = if m.input.trim().is_empty() {
        "Memory filesystem"
    } else {
        "Memory (search)"
    };
    result_list::render(frame, area, title, &entries, m.selected);
}

/// A one-line preview for the list sub-label.
fn entry_preview(entry: &MemoryEntry) -> Option<String> {
    let text = match entry {
        MemoryEntry::Item { item, .. } => item.content(),
        MemoryEntry::Node { node, .. } => node.abstract_(),
    };
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        None
    } else {
        Some(single_line)
    }
}

fn render_detail(frame: &mut Frame, area: Rect, state: &AppState) {
    let m = &state.memory;

    let selected = if m.error.is_some() {
        None
    } else {
        m.entries.get(m.selected)
    };

    let focused = m.focused_pane == MemoryPane::Detail;
    let border_color = if focused { Color::Cyan } else { Color::White };

    let (title, body) = match selected {
        Some(entry) => (detail_title(entry), detail_body(entry)),
        None => {
            let block = Block::default()
                .title("  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                Paragraph::new("  No memory selected.")
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

    let para = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .scroll((m.detail_scroll, 0));
    frame.render_widget(para, inner);
}

fn detail_title(entry: &MemoryEntry) -> String {
    match entry {
        MemoryEntry::Item { item, .. } => format!("{} / {}", item.kind(), item.name()),
        MemoryEntry::Node { node, .. } => node.uri().to_string(),
    }
}

/// Full detail body: item content, or the node's L0/L1/L2 sections.
fn detail_body(entry: &MemoryEntry) -> String {
    match entry {
        MemoryEntry::Item { item, .. } => {
            format!(
                "[{}] {}\nupdated {} time(s), source session: {}\n\n{}",
                item.kind(),
                item.name(),
                item.update_count(),
                item.source_session_id().unwrap_or("(unknown)"),
                item.content()
            )
        }
        MemoryEntry::Node { node, .. } => {
            let mut out = format!("[{}] {}\n\n", node.kind(), node.uri());
            out.push_str(&format!("## Abstract (L0)\n{}\n", node.abstract_()));
            if !node.overview().trim().is_empty() {
                out.push_str(&format!("\n## Overview (L1)\n{}\n", node.overview()));
            }
            if !node.content().trim().is_empty() {
                out.push_str(&format!("\n## Detail (L2)\n{}\n", node.content()));
            }
            out
        }
    }
}
