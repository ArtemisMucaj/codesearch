use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::state::{ActiveMode, AppState};

/// Renders the top bar containing mode tabs and the text input field.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let (search_style, impact_style, context_style) = tab_styles(&state.mode);

    let title = Line::from(vec![
        Span::styled(" Search ", search_style),
        Span::raw("  "),
        Span::styled(" Impact ", impact_style),
        Span::raw("  "),
        Span::styled(" Context ", context_style),
        Span::raw(" "),
    ]);

    let loading_indicator = if state.is_loading() { " ⟳" } else { "" };
    let input_text = format!("> {}{}", state.active_input(), loading_indicator);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(input_text).block(block);
    frame.render_widget(paragraph, area);

    // Show the terminal cursor inside the input box.
    // Layout: border (1) + "> " prefix (2) + cursor offset.
    let cursor_x = area.x + 1 + 2 + state.active_cursor() as u16;
    let cursor_y = area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn tab_styles(mode: &ActiveMode) -> (Style, Style, Style) {
    let active = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(Color::DarkGray);

    match mode {
        ActiveMode::Search => (active, inactive, inactive),
        ActiveMode::Impact => (inactive, active, inactive),
        ActiveMode::Context => (inactive, inactive, active),
    }
}
