use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::application::ImpactAnalysis;

/// Render an icicle chart for the impact analysis result.
///
/// The root symbol occupies the top row; its direct callers are shown on the
/// second row; their callers on the third, and so on — mirroring how a flame
/// graph grows upward from a call site.  Each symbol is rendered as a
/// coloured block whose width is proportional to the terminal area divided
/// by the number of siblings at that depth.
pub fn render(frame: &mut Frame, area: Rect, analysis: &ImpactAnalysis, scroll: u16) {
    let block = Block::default()
        .title(" Blast Radius ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let max_depth = analysis.max_depth_reached;

    let mut all_lines: Vec<Line> = Vec::new();

    // Row 0 — root symbol
    all_lines.push(build_row(&[analysis.root_symbol.as_str()], width, 0, max_depth));

    // Rows 1..=max_depth — callers at each depth (skip empty buckets)
    for (i, nodes) in analysis.by_depth.iter().enumerate() {
        if nodes.is_empty() {
            continue;
        }
        let depth = i + 1;
        let symbols: Vec<&str> = nodes.iter().map(|n| n.symbol.as_str()).collect();
        all_lines.push(build_row(&symbols, width, depth, max_depth));
    }

    // Apply scroll and render
    let visible: Vec<Line> = all_lines
        .into_iter()
        .skip(scroll as usize)
        .take(inner.height as usize)
        .collect();

    let para = Paragraph::new(visible);
    frame.render_widget(para, inner);
}

fn build_row<'a>(symbols: &[&'a str], width: usize, depth: usize, max_depth: usize) -> Line<'a> {
    let count = symbols.len().max(1);
    // Each cell gets an equal share; minimum 3 chars so labels are never invisible.
    let cell_w = (width / count).max(3);
    let color = depth_color(depth, max_depth);

    let spans: Vec<Span> = symbols
        .iter()
        .map(|sym| {
            let label = truncate_middle(sym, cell_w.saturating_sub(2));
            // Centre the label inside the brackets.
            let padded = format!("{:^width$}", label, width = cell_w.saturating_sub(2));
            Span::styled(
                format!("[{}]", padded),
                Style::default()
                    .bg(color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();

    Line::from(spans)
}

/// Map depth to a colour gradient: root = red, middle = yellow, leaves = green.
fn depth_color(depth: usize, max_depth: usize) -> Color {
    if max_depth == 0 {
        return Color::Red;
    }
    let ratio = depth as f32 / max_depth as f32;
    if ratio < 0.33 {
        Color::Red
    } else if ratio < 0.66 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Shorten `s` to at most `max_chars`, replacing the middle with `…` when needed.
fn truncate_middle(s: &str, max_chars: usize) -> String {
    if max_chars < 3 {
        return ".".repeat(max_chars.min(1));
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars - 1; // 1 char for …
    let left = keep / 2;
    let right = keep - left;
    let l: String = chars[..left].iter().collect();
    let r: String = chars[chars.len() - right..].iter().collect();
    format!("{}…{}", l, r)
}
