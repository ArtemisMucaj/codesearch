mod context;
mod format;
mod impact;
mod search;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::state::{ActiveMode, AppState};
use crate::tui::widgets::input_bar;

/// Entry point called on every terminal draw cycle.
pub fn render(frame: &mut Frame, state: &AppState) {
    let areas = Layout::vertical([
        Constraint::Length(3), // tab bar + input
        Constraint::Min(0),    // main content
        Constraint::Length(1), // status bar
    ])
    .split(frame.area());

    input_bar::render(frame, areas[0], state);

    match state.mode {
        ActiveMode::Search => search::render(frame, areas[1], state),
        ActiveMode::Impact => impact::render(frame, areas[1], state),
        ActiveMode::Context => context::render(frame, areas[1], state),
    }

    render_status(frame, areas[2], state);
}

fn render_status(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let hints = match state.mode {
        ActiveMode::Search => {
            " Enter: search  ↑↓: navigate  Ctrl+↑: impact on symbol  Tab: switch  PgUp/Dn: scroll  q: quit "
        }
        ActiveMode::Impact => {
            " Enter: analyse  ↑↓: navigate  Tab: switch  PgUp/Dn: scroll  q: quit "
        }
        ActiveMode::Context => {
            " Enter: lookup  ↑↓: navigate  ←→: callers/callees  Tab: switch  PgUp/Dn: scroll  q: quit "
        }
    };

    let status = Paragraph::new(hints)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(status, area);
}
