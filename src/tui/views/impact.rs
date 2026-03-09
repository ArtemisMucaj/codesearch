use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::application::ImpactNode;
use crate::tui::state::{AppState, ImpactPane};
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;

use super::format::{short_symbol, shorten_path};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes = Layout::horizontal([
        Constraint::Percentage(35),
        Constraint::Percentage(65),
    ])
    .split(area);

    render_entry_points(frame, panes[0], state);
    render_right(frame, panes[1], state);
}

// ── Left pane: entry-point list ───────────────────────────────────────────────

fn render_entry_points(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.impact;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Entrypoints ")
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

    let entries: Vec<ListEntry> = match &s.analysis {
        None => vec![],
        Some(analysis) => analysis
            .leaf_nodes()
            .into_iter()
            .map(|node| ListEntry {
                label: format!("{}:{}", shorten_path(&node.file_path), node.line),
                sub_label: Some(short_symbol(&node.symbol).to_string()),
                score: None,
            })
            .collect(),
    };

    let title = format!("Entrypoints ({})", entries.len());
    result_list::render(frame, area, &title, &entries, s.selected);
}

// ── Right pane: call chain or code view ───────────────────────────────────────

fn render_right(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.impact;
    let chain_focused = s.focused_pane == ImpactPane::Chain;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Call tree ")
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

    let analysis = match &s.analysis {
        None => {
            let block = Block::default()
                .title(" Call tree ")
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
            return;
        }
        Some(a) => a,
    };

    let leaves = analysis.leaf_nodes();
    if leaves.is_empty() {
        let block = Block::default()
            .title(" Call tree ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(
            Paragraph::new("  No callers found.")
                .block(block)
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let leaf = leaves.get(s.selected).copied().unwrap_or(leaves[0]);
    let path = analysis.path_for_leaf(leaf);

    // Code view: if a chain node snippet is loaded or loading, show it.
    if s.chain_snippet_loading {
        let border_color = if chain_focused { Color::Cyan } else { Color::White };
        let block = Block::default()
            .title(" Code ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        frame.render_widget(
            Paragraph::new("  Loading…")
                .block(block)
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    if let Some(chunk) = &s.chain_snippet {
        let border_color = if chain_focused { Color::Cyan } else { Color::White };
        let title = format!(" {} ", shorten_path(chunk.file_path()));
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        // Render manually so we can apply our border color.
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = chunk
            .content()
            .lines()
            .enumerate()
            .map(|(i, line)| {
                let lineno = chunk.start_line() as usize + i;
                Line::from(vec![
                    Span::styled(
                        format!("{:>4}  ", lineno),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(line.to_string()),
                ])
            })
            .collect();

        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((s.chain_snippet_scroll, 0));
        frame.render_widget(para, inner);

        // Hint at the bottom of the status bar is set in views/mod.rs.
        return;
    }

    // Default: call chain with node selection.
    let border_color = if chain_focused { Color::Cyan } else { Color::White };
    render_path_tree(
        frame,
        area,
        &path,
        &analysis.root_symbol,
        chain_focused,
        s.chain_selected,
        s.flame_scroll,
        border_color,
    );
}

// ── Path tree renderer ────────────────────────────────────────────────────────

/// Render the call chain entry-point-first, root-last:
///
/// ```text
/// ★  entry_point  file:line       ← index 0, selectable
///    │
///    └── intermediate  file:line  ← index 1, selectable
///        │
///        └── ◉  root_symbol
/// ```
fn render_path_tree(
    frame: &mut Frame,
    area: Rect,
    path: &[&ImpactNode], // leaf-first order
    root_symbol: &str,
    chain_focused: bool,
    selected: usize,
    scroll: u16,
    border_color: Color,
) {
    let block = Block::default()
        .title(" Call tree ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // First row: entry point (path[0]).
    if let Some(leaf) = path.first() {
        let is_sel = chain_focused && selected == 0;
        let fg = if is_sel { Color::Black } else { Color::Cyan };
        let bg = if is_sel { Color::Cyan } else { Color::Reset };
        let marker = if is_sel { "▶ ★  " } else { "  ★  " };

        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan).bg(bg)),
            Span::styled(
                short_symbol(&leaf.symbol).to_string(),
                Style::default()
                    .fg(fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:{}", shorten_path(&leaf.file_path), leaf.line),
                Style::default().fg(if is_sel { Color::Black } else { Color::DarkGray }).bg(bg),
            ),
        ]));
    }

    // Intermediate nodes (path[1..]).
    for (idx, node) in path.iter().skip(1).enumerate() {
        let node_idx = idx + 1;
        let is_sel = chain_focused && selected == node_idx;
        let base_indent = "    ".repeat(idx);

        lines.push(Line::from(Span::styled(
            format!("{}   │", base_indent),
            Style::default().fg(Color::DarkGray),
        )));

        let fg = if is_sel { Color::Black } else { Color::White };
        let bg = if is_sel { Color::White } else { Color::Reset };
        let marker = if is_sel { "▶ " } else { "  " };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{}   └── {}", base_indent, marker),
                Style::default().fg(Color::DarkGray).bg(bg),
            ),
            Span::styled(
                short_symbol(&node.symbol).to_string(),
                Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:{}", shorten_path(&node.file_path), node.line),
                Style::default().fg(if is_sel { Color::Black } else { Color::DarkGray }).bg(bg),
            ),
        ]));
    }

    // Final row: root symbol (◉) — not selectable.
    {
        let depth = path.len().saturating_sub(1);
        let base_indent = "    ".repeat(depth);
        lines.push(Line::from(Span::styled(
            format!("{}   │", base_indent),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}   └── ", base_indent),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("◉  ", Style::default().fg(Color::Red)),
            Span::styled(
                root_symbol.to_string(),
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll as usize)
        .take(inner.height as usize)
        .collect();

    frame.render_widget(Paragraph::new(visible), inner);
}
