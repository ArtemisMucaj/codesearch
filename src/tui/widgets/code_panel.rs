use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// Render an indexed code chunk in a bordered panel with line numbers.
///
/// `start_line` is the 1-based line number of the first line of `content`
/// so that line numbers shown in the panel align with those reported by
/// search results and call-graph edges.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    content: &str,
    start_line: u32,
    scroll: u16,
) {
    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));

    if content.is_empty() {
        let placeholder = Paragraph::new("  No snippet available.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(placeholder, area);
        return;
    }

    let lines: Vec<Line> = content
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let lineno = start_line as usize + i;
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
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(para, area);
}
