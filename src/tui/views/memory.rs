use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::{MemoryLevel, MemoryRow, RowTarget};
use crate::tui::state::{AppState, MemoryPane};
use crate::tui::widgets::markdown;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // Match the other modes' pane split so tabbing between views is seamless.
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    render_tree(frame, panes[0], state);
    render_detail(frame, panes[1], state);
}

// ── Left pane: the filesystem tree (browse) or ranked hits (search) ───────────

fn render_tree(frame: &mut Frame, area: Rect, state: &AppState) {
    let m = &state.memory;

    let searching = !m.input.trim().is_empty();
    let title = if searching {
        format!(" Memory (search) ({}) ", m.entries.len())
    } else {
        format!(" Memory filesystem ({}) ", m.entries.len())
    };

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

    let border_style = if m.entries.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if m.entries.is_empty() {
        let hint = if searching {
            "  No matches."
        } else {
            "  No memories yet. Import a session or `memory add` a resource."
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = m
        .entries
        .iter()
        .enumerate()
        .map(|(i, row)| row_line(row, i == m.selected, searching))
        .collect();

    // Keep the selected row visible with a simple scroll window.
    let height = inner.height as usize;
    let scroll = if m.selected >= height {
        m.selected + 1 - height
    } else {
        0
    };
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Render one tree row: indentation + a kind glyph + label (+ score badge).
fn row_line(row: &MemoryRow, selected: bool, searching: bool) -> Line<'static> {
    let indent = "  ".repeat(row.depth as usize);
    let (glyph, label_color) = match &row.target {
        RowTarget::Directory => ("▾ ", Color::Blue),
        RowTarget::Node(_) => (node_glyph(&row.kind_label), Color::Cyan),
        RowTarget::NodeLevel { .. } => ("└─ ", Color::Green),
        RowTarget::Item(_) => ("• ", Color::White),
    };

    let bg = if selected {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let name_style = if selected {
        Style::default()
            .fg(label_color)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(label_color).bg(bg)
    };

    // For nodes/items in the tree, prefix the kind; level rows already carry it.
    let text = match &row.target {
        RowTarget::Node(_) | RowTarget::Item(_) if !row.kind_label.is_empty() => {
            format!("[{}] {}", row.kind_label, row.label)
        }
        _ => row.label.clone(),
    };

    let mut spans = vec![
        Span::styled(
            format!("{indent}{glyph}"),
            Style::default().fg(Color::DarkGray).bg(bg),
        ),
        Span::styled(text, name_style),
    ];
    if searching {
        if let Some(score) = row.score {
            spans.push(Span::styled(
                format!("  {:.2}", score),
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
        }
    }
    Line::from(spans)
}

fn node_glyph(kind_label: &str) -> &'static str {
    match kind_label {
        "memory" => "★ ", // the rollup — read this first
        _ => "◆ ",        // session / resource node
    }
}

// ── Right pane: detail for the selected row ───────────────────────────────────

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
        Some(row) => (detail_title(row), detail_body(row)),
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

fn detail_title(row: &MemoryRow) -> String {
    match &row.target {
        RowTarget::Directory => row.label.clone(),
        RowTarget::Node(node) => node.uri().to_string(),
        RowTarget::NodeLevel { node, level } => format!("{}  ·  {}", node.uri(), level.tag()),
        RowTarget::Item(item) => format!("{} / {}", item.kind(), item.name()),
    }
}

/// Build the styled detail for the selected row. A level row shows just that
/// level; a node row shows its L0+L1 summary (drill into the L2 child row for
/// the full body); an item shows its content.
fn detail_body(row: &MemoryRow) -> Vec<Line<'static>> {
    match &row.target {
        RowTarget::Directory => {
            vec![Line::from(Span::styled(
                "Directory — select a child to view it.",
                Style::default().fg(Color::DarkGray),
            ))]
        }
        RowTarget::Item(item) => {
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
        RowTarget::NodeLevel { node, level } => {
            let (tag, text) = match level {
                MemoryLevel::Abstract => ("L0 · Abstract", node.abstract_()),
                MemoryLevel::Overview => ("L1 · Overview", node.overview()),
                MemoryLevel::Detail => ("L2 · Detail", node.content()),
            };
            let mut lines = vec![section_header(tag), Line::from("")];
            lines.extend(markdown::render(text));
            lines
        }
        RowTarget::Node(node) => {
            // The node row is the summary view: L0 + L1 only. The full L2 body
            // (transcript / resource text) is reached by selecting its own
            // "L2 · detail" child row, so a node preview stays scannable.
            let mut lines = Vec::new();
            lines.push(section_header("L0 · Abstract"));
            lines.extend(markdown::render(node.abstract_()));
            if !node.overview().trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(section_header("L1 · Overview"));
                lines.extend(markdown::render(node.overview()));
            }
            if !node.content().trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(meta_line(
                    "(select \"L2 · detail\" to read the full content)",
                ));
            }
            lines
        }
    }
}

/// A styled section header delineating an L0/L1/L2 level.
fn section_header(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("▍ {label}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn meta_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::MemoryRow;
    use crate::domain::{MemoryNode, NodeKind};
    use crate::tui::state::{ActiveMode, AppState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn node(uri: &str, kind: NodeKind, overview: &str, content: &str) -> MemoryNode {
        MemoryNode::new(
            uri.into(),
            kind,
            None,
            "the abstract".into(),
            overview.into(),
            content.into(),
            0,
            0,
        )
    }

    /// Render the tree to a headless backend and return the plain-text buffer.
    fn render_to_text(rows: Vec<MemoryRow>, selected: usize) -> String {
        let mut state = AppState::new(None, ActiveMode::Memory, None, true);
        state.memory.entries = rows;
        state.memory.selected = selected;

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), &state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Flatten the cell grid to text, row by row.
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_tree_with_nested_levels() {
        let sess = node(
            "memory://sessions/abc",
            NodeKind::Session,
            "an overview",
            "transcript body",
        );
        let rows = vec![
            MemoryRow {
                depth: 0,
                kind_label: String::new(),
                label: "sessions/".into(),
                preview: None,
                score: None,
                target: RowTarget::Directory,
            },
            MemoryRow {
                depth: 1,
                kind_label: "session".into(),
                label: sess.uri().into(),
                preview: None,
                score: None,
                target: RowTarget::Node(sess.clone()),
            },
            MemoryRow {
                depth: 2,
                kind_label: String::new(),
                label: "L0 · abstract".into(),
                preview: None,
                score: None,
                target: RowTarget::NodeLevel {
                    node: sess.clone(),
                    level: MemoryLevel::Abstract,
                },
            },
        ];

        // Selecting the L0 level row shows only that level on the right.
        let text = render_to_text(rows, 2);
        assert!(text.contains("Memory filesystem"), "list title present");
        assert!(text.contains("sessions/"), "directory row rendered");
        assert!(text.contains("memory://sessions/abc"), "node row rendered");
        assert!(text.contains("L0"), "nested level row rendered");
        // Detail pane shows the abstract for the selected L0 level.
        assert!(
            text.contains("the abstract"),
            "L0 detail shown on the right"
        );
        // And NOT the L2 transcript, since only L0 is selected.
        assert!(
            !text.contains("transcript body"),
            "L2 content should not appear when only L0 is selected"
        );
    }

    #[test]
    fn selecting_node_row_shows_l0_l1_only() {
        let sess = node(
            "memory://sessions/xyz",
            NodeKind::Session,
            "the overview",
            "the transcript",
        );
        let rows = vec![MemoryRow {
            depth: 0,
            kind_label: "session".into(),
            label: sess.uri().into(),
            preview: None,
            score: None,
            target: RowTarget::Node(sess.clone()),
        }];
        let text = render_to_text(rows, 0);
        // Node row selected → detail shows L0 + L1 (the summary), not the L2 body.
        assert!(text.contains("L0"), "L0 section header");
        assert!(text.contains("L1"), "L1 section header");
        assert!(text.contains("the abstract"), "L0 body shown");
        assert!(text.contains("the overview"), "L1 body shown");
        assert!(
            !text.contains("the transcript"),
            "L2 body should NOT appear on the node row (drill into L2 row instead)"
        );
    }
}
