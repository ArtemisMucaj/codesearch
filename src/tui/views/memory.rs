use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::MemoryEntry;
use crate::tui::state::{AppState, MemoryPane};
use crate::tui::widgets::markdown;
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // Match the other modes' pane split so tabbing between views is seamless.
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

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

/// Build the styled detail body. Items render their Markdown content; nodes
/// render their L0/L1/L2 as separate, labelled sections (child levels) rather
/// than one raw Markdown blob.
fn detail_body(entry: &MemoryEntry) -> Vec<Line<'static>> {
    match entry {
        MemoryEntry::Item { item, .. } => {
            let mut lines = vec![
                meta_line(&format!(
                    "updated {}×  ·  source: {}",
                    item.update_count(),
                    item.source_session_id().unwrap_or("(unknown)")
                )),
                Line::from(""),
            ];
            lines.extend(markdown::render(item.content()));
            lines
        }
        MemoryEntry::Node { node, .. } => {
            let mut lines = Vec::new();

            // L0 — Abstract (always present).
            lines.push(section_header("L0 · Abstract"));
            lines.extend(markdown::render(node.abstract_()));

            // L1 — Overview (when present).
            if !node.overview().trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(section_header("L1 · Overview"));
                lines.extend(markdown::render(node.overview()));
            }

            // L2 — Detail (when present, e.g. a session transcript or resource).
            if !node.content().trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(section_header("L2 · Detail"));
                lines.extend(markdown::render(node.content()));
            }

            lines
        }
    }
}

/// A styled section header used to delineate the L0/L1/L2 child levels.
fn section_header(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("▍ {label}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A dim single-line metadata row.
fn meta_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}
