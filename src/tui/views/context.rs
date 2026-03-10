use std::collections::{HashMap, HashSet};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::{ContextNode, SymbolContext};
use crate::tui::state::{AppState, ContextPane};
use crate::tui::widgets::result_list;
use crate::tui::widgets::result_list::ListEntry;
use crate::tui::widgets::syntax;

use super::format::{short_symbol, shorten_path};

/// Public entry point called by `views/mod.rs`.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    render_entry_points(frame, panes[0], state);
    render_right(frame, panes[1], state);
}

// ── Left pane: entry-point list ───────────────────────────────────────────────

fn render_entry_points(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Entry-points ")
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

    let entries: Vec<ListEntry> = match &s.context {
        None => vec![],
        Some(ctx) => {
            let leaves = leaf_caller_nodes(ctx);
            if leaves.is_empty() {
                // No callers: show a synthetic "callees only" entry.
                vec![ListEntry {
                    label: format!("◉  {}", short_symbol(&ctx.symbol)),
                    sub_label: Some("no callers — callees only".to_string()),
                    score: None,
                }]
            } else {
                leaves
                    .iter()
                    .map(|node| ListEntry {
                        label: format!("{}:{}", shorten_path(&node.file_path), node.line),
                        sub_label: Some(short_symbol(&node.symbol).to_string()),
                        score: None,
                    })
                    .collect()
            }
        }
    };

    let title = format!("Entry-points ({})", entries.len());
    result_list::render(frame, area, &title, &entries, s.selected);
}

// ── Right pane: call context tree or code view ────────────────────────────────

fn render_right(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;
    let tree_focused = s.focused_pane == ContextPane::Tree;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Call context ")
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

    let ctx = match &s.context {
        None => {
            let block = Block::default()
                .title(" Call context ")
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
        Some(c) => c,
    };

    // Code view: if a chain node snippet is loaded or loading, show it.
    if s.chain_snippet_loading {
        let border_color = if tree_focused {
            Color::Cyan
        } else {
            Color::White
        };
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
        let border_color = if tree_focused {
            Color::Cyan
        } else {
            Color::White
        };
        let title = format!(" {} ", shorten_path(chunk.file_path()));
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = syntax::highlight_code(
            chunk.content(),
            chunk.file_path(),
            chunk.start_line() as usize,
        );

        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((s.chain_snippet_scroll, 0));
        frame.render_widget(para, inner);
        return;
    }

    // Default: call context tree.
    let border_color = if tree_focused {
        Color::Cyan
    } else {
        Color::White
    };

    let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
    let leaves = leaf_caller_nodes(ctx);

    let (path, has_callers) = if leaves.is_empty() {
        (vec![], false)
    } else {
        let leaf = leaves.get(s.selected).copied().unwrap_or(leaves[0]);
        let path = trace_caller_path(leaf, &all_callers);
        (path, true)
    };

    let callee_children = build_callee_children_map(ctx);

    render_call_context_tree(
        frame,
        area,
        &path,
        &ctx.symbol,
        &callee_children,
        has_callers,
        tree_focused,
        s.chain_selected,
        s.tree_scroll,
        border_color,
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the top-most entry-point nodes from the callers BFS.
///
/// A node is a leaf (entry-point) if no other caller node lists it as its
/// `via_symbol`.
pub fn leaf_caller_nodes(ctx: &SymbolContext) -> Vec<&ContextNode> {
    let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
    all_callers
        .iter()
        .copied()
        .filter(|n| {
            !all_callers
                .iter()
                .any(|m| m.via_symbol.as_deref() == Some(n.symbol.as_str()))
        })
        .collect()
}

/// Build the caller path from a leaf node back to the direct caller of the root symbol.
///
/// Returns a Vec where index 0 is the leaf (top-most caller), last index is the
/// direct caller of the root symbol.
pub fn trace_caller_path<'a>(
    leaf: &'a ContextNode,
    all_callers: &[&'a ContextNode],
) -> Vec<&'a ContextNode> {
    let node_by_depth_sym: HashMap<(usize, &str), &ContextNode> = all_callers
        .iter()
        .map(|n| ((n.depth, n.symbol.as_str()), *n))
        .collect();

    let mut path = vec![leaf];
    let mut current = leaf;
    while let Some(via) = current.via_symbol.as_deref() {
        let parent_depth = current.depth.saturating_sub(1);
        if let Some(&parent) = node_by_depth_sym.get(&(parent_depth, via)) {
            path.push(parent);
            current = parent;
        } else {
            break;
        }
    }
    path
}

/// Build a map from parent_symbol → direct callee ContextNodes.
fn build_callee_children_map<'a>(ctx: &'a SymbolContext) -> HashMap<String, Vec<&'a ContextNode>> {
    let mut map: HashMap<String, Vec<&'a ContextNode>> = HashMap::new();
    for node in ctx.callees_by_depth.iter().flatten() {
        let key = node.via_symbol.as_deref().unwrap_or(&ctx.symbol).to_owned();
        map.entry(key).or_default().push(node);
    }
    map
}

// ── Tree renderer ─────────────────────────────────────────────────────────────

/// Render the full call context tree:
///
/// ```text
/// ★  entry_point  file:line         ← selectable (index 0)
///    │
///    └── intermediate  file:line    ← selectable (index 1)
///        │
///        └── ◉  root_symbol         ← NOT selectable
///            ├── child_A  file:line ← NOT selectable (callee)
///            └── child_B  file:line ← NOT selectable (callee)
/// ```
#[allow(clippy::too_many_arguments)]
fn render_call_context_tree(
    frame: &mut Frame,
    area: Rect,
    path: &[&ContextNode],
    root_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    has_callers: bool,
    tree_focused: bool,
    selected: usize,
    scroll: u16,
    border_color: Color,
) {
    let block = Block::default()
        .title(" Call context ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if has_callers {
        // ── Caller chain ──────────────────────────────────────────────────────

        if let Some(leaf) = path.first() {
            let is_sel = tree_focused && selected == 0;
            let fg = if is_sel { Color::Black } else { Color::Cyan };
            let bg = if is_sel { Color::Cyan } else { Color::Reset };
            let marker = if is_sel { "▶ ★  " } else { "  ★  " };

            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan).bg(bg)),
                Span::styled(
                    short_symbol(&leaf.symbol).to_string(),
                    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}:{}", shorten_path(&leaf.file_path), leaf.line),
                    Style::default()
                        .fg(if is_sel {
                            Color::Black
                        } else {
                            Color::DarkGray
                        })
                        .bg(bg),
                ),
            ]));
        }

        for (idx, node) in path.iter().skip(1).enumerate() {
            let node_idx = idx + 1;
            let is_sel = tree_focused && selected == node_idx;
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
                    Style::default()
                        .fg(if is_sel {
                            Color::Black
                        } else {
                            Color::DarkGray
                        })
                        .bg(bg),
                ),
            ]));
        }

        // Root symbol (◉) — not selectable.
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
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            ]));

            // Callee subtree hangs off the root symbol.
            let callee_prefix = "    ".repeat(depth + 1);
            let mut visited: HashSet<String> = HashSet::new();
            render_callees_subtree(
                root_symbol,
                callee_children,
                &callee_prefix,
                &mut lines,
                &mut visited,
            );
        }
    } else {
        // No callers: just root symbol + callees.
        lines.push(Line::from(vec![
            Span::styled("◉  ", Style::default().fg(Color::Red)),
            Span::styled(
                root_symbol.to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]));
        let mut visited: HashSet<String> = HashSet::new();
        render_callees_subtree(
            root_symbol,
            callee_children,
            "    ",
            &mut lines,
            &mut visited,
        );
    }

    let visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll as usize)
        .take(inner.height as usize)
        .collect();

    frame.render_widget(Paragraph::new(visible), inner);
}

/// Recursively render the callees subtree rooted at `parent_symbol`.
fn render_callees_subtree(
    parent_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    prefix: &str,
    lines: &mut Vec<Line>,
    visited: &mut HashSet<String>,
) {
    let children: &Vec<&ContextNode> = match callee_children.get(parent_symbol) {
        Some(c) => c,
        None => return,
    };
    let count = children.len();
    for (i, node) in children.iter().enumerate() {
        if !visited.insert(node.symbol.clone()) {
            continue; // cycle guard
        }
        let is_last = i == count - 1;
        let branch = if is_last { "└──" } else { "├──" };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{} ", prefix, branch),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                short_symbol(&node.symbol).to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:{}", shorten_path(&node.file_path), node.line),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        let child_prefix = if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };
        render_callees_subtree(&node.symbol, callee_children, &child_prefix, lines, visited);
    }
}
