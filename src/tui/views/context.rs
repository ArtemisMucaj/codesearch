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

    let title = "Entrypoints";
    result_list::render(frame, area, title, &entries, s.selected);
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

    // Record the inner pane height so the navigator can auto-scroll.
    // We compute it here (same formula as the renderer uses internally).
    let inner_height = area.height.saturating_sub(2); // subtract top+bottom border
    state.context.tree_pane_height.set(inner_height);

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

/// Convenience: build the flat tree node list for the currently selected entry-point.
///
/// Returns `None` if the context hasn't loaded yet.
pub fn build_flat_tree_for_selected(ctx: &SymbolContext, selected_entry: usize) -> Vec<FlatNode> {
    let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();
    let leaves = leaf_caller_nodes(ctx);
    let callee_children = build_callee_children_map(ctx);

    let (path, has_callers) = if leaves.is_empty() {
        (vec![], false)
    } else {
        let leaf = leaves.get(selected_entry).copied().unwrap_or(leaves[0]);
        let p = trace_caller_path(leaf, &all_callers);
        (p, true)
    };

    flat_tree_nodes(&path, &ctx.symbol, &callee_children, has_callers)
}

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

/// A selectable node in the flattened tree.
#[derive(Debug, Clone)]
pub struct FlatNode {
    pub symbol: String,
    pub repository_id: String,
    pub file_path: String,
    pub line: u32,
    /// The `lines` index this node occupies in the rendered tree (for auto-scroll).
    pub lines_index: usize,
    /// `true` when this node is a callee of the root symbol (i.e. below ◉).
    /// Callee snippets are looked up by symbol name rather than call-site location.
    pub is_callee: bool,
}

/// Flatten the full context tree into a single ordered list of selectable nodes.
///
/// Order: caller chain (leaf → direct caller), then root ◉ (not included — not
/// selectable), then callee nodes in DFS order.
///
/// `path` is the caller chain for the currently selected entry-point (leaf-first).
/// `callee_children` is the map built by `build_callee_children_map`.
/// `has_callers` mirrors the same flag passed to the tree renderer.
///
/// Returns `(flat_nodes, lines_per_caller_node)` where `lines_per_caller_node`
/// encodes how many rendered lines come before each caller in the `lines` vec.
pub fn flat_tree_nodes(
    path: &[&ContextNode],
    root_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    has_callers: bool,
) -> Vec<FlatNode> {
    let mut result = Vec::new();

    if has_callers {
        // ── Caller nodes ──────────────────────────────────────────────────────
        // path[0] (leaf): lines[0]
        // path[i] (i≥1): lines[i*2]  (each preceded by one connector line)
        for (i, node) in path.iter().enumerate() {
            let lines_index = if i == 0 { 0 } else { i * 2 };
            result.push(FlatNode {
                symbol: node.symbol.clone(),
                repository_id: node.repository_id.clone(),
                file_path: node.file_path.clone(),
                line: node.line,
                lines_index,
                is_callee: false,
            });
        }

        // ◉ root is at lines[path.len() * 2] — not selectable, skip.
        // Callees start at lines[path.len() * 2 + 1].
        let callee_start_lines = path.len() * 2 + 1;
        let mut callee_offset = 0usize;
        let mut visited: HashSet<String> = HashSet::new();
        collect_flat_callees(
            root_symbol,
            callee_children,
            callee_start_lines,
            &mut callee_offset,
            &mut result,
            &mut visited,
        );
    } else {
        // ◉ root is at lines[0] — not selectable.
        // Callees start at lines[1].
        let callee_start_lines = 1;
        let mut callee_offset = 0usize;
        let mut visited: HashSet<String> = HashSet::new();
        collect_flat_callees(
            root_symbol,
            callee_children,
            callee_start_lines,
            &mut callee_offset,
            &mut result,
            &mut visited,
        );
    }

    result
}

fn collect_flat_callees(
    parent_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    base_lines_index: usize,
    offset: &mut usize,
    result: &mut Vec<FlatNode>,
    visited: &mut HashSet<String>,
) {
    let children = match callee_children.get(parent_symbol) {
        Some(c) => c,
        None => return,
    };
    for node in children.iter() {
        if !visited.insert(node.symbol.clone()) {
            continue;
        }
        result.push(FlatNode {
            symbol: node.symbol.clone(),
            repository_id: node.repository_id.clone(),
            file_path: node.file_path.clone(),
            line: node.line,
            lines_index: base_lines_index + *offset,
            is_callee: true,
        });
        *offset += 1;
        collect_flat_callees(
            &node.symbol,
            callee_children,
            base_lines_index,
            offset,
            result,
            visited,
        );
    }
}

// ── Tree renderer ─────────────────────────────────────────────────────────────

/// Render the full call context tree:
///
/// ```text
/// ★  entry_point  file:line         ← flat index 0
///    │
///    └── intermediate  file:line    ← flat index 1
///        │
///        └── ◉  root_symbol         ← not selectable
///            ├── child_A  file:line ← flat index path.len()
///            └── child_B  file:line ← flat index path.len()+1
/// ```
///
/// `selected` is a flat index across callers (0..path.len()-1) then callees.
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

    // callee_flat_idx: the flat index that the first callee node gets.
    let callee_flat_base = if has_callers { path.len() } else { 0 };

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
            let mut callee_counter = 0usize;
            render_callees_subtree(
                root_symbol,
                callee_children,
                &callee_prefix,
                &mut lines,
                &mut visited,
                tree_focused,
                selected,
                callee_flat_base,
                &mut callee_counter,
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
        let mut callee_counter = 0usize;
        render_callees_subtree(
            root_symbol,
            callee_children,
            "    ",
            &mut lines,
            &mut visited,
            tree_focused,
            selected,
            callee_flat_base,
            &mut callee_counter,
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
///
/// `flat_base` is the flat index of the first callee node overall.
/// `counter` tracks how many callee nodes have been rendered so far (DFS order).
/// A callee node at DFS position `*counter` has flat index `flat_base + *counter`.
#[allow(clippy::too_many_arguments)]
fn render_callees_subtree(
    parent_symbol: &str,
    callee_children: &HashMap<String, Vec<&ContextNode>>,
    prefix: &str,
    lines: &mut Vec<Line>,
    visited: &mut HashSet<String>,
    tree_focused: bool,
    selected: usize,
    flat_base: usize,
    counter: &mut usize,
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

        let flat_idx = flat_base + *counter;
        *counter += 1;

        let is_sel = tree_focused && selected == flat_idx;
        let fg_name = if is_sel { Color::Black } else { Color::Yellow };
        let bg = if is_sel { Color::Yellow } else { Color::Reset };
        let marker = if is_sel { "▶ " } else { "" };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{} {}", prefix, branch, marker),
                Style::default().fg(Color::DarkGray).bg(bg),
            ),
            Span::styled(
                short_symbol(&node.symbol).to_string(),
                Style::default()
                    .fg(fg_name)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
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

        let child_prefix = if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };
        render_callees_subtree(
            &node.symbol,
            callee_children,
            &child_prefix,
            lines,
            visited,
            tree_focused,
            selected,
            flat_base,
            counter,
        );
    }
}
