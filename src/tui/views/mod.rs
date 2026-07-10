pub(crate) mod context;
mod format;
mod impact;
mod memory;
mod search;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

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
        ActiveMode::Memory => memory::render(frame, areas[1], state),
    }

    render_status(frame, areas[2], state);
}

fn render_status(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    // While the ONNX models are still loading in the background, replace the
    // normal keybinding hints with a loading notice so the user knows why
    // Enter doesn't work yet.
    if !state.models_ready {
        let status = Paragraph::new(" Models loading… (you can start typing)")
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(status, area);
        return;
    }

    // One consistent shortcut bar across every mode and pane, so the footer
    // never shifts under the user. The keys mean the analogous thing everywhere:
    // Enter = primary action (open/analyze/view), Esc = back, Tab = switch mode.
    // In Search's code pane, `I`/`X` additionally jump to Impact/Context.
    const HINTS: &str =
        " Tab: mode  ↑↓: navigate  Enter: open/analyze  Esc: back  I/X: impact/context  PgUp/Dn: scroll  Ctrl+C: quit";

    let status = Paragraph::new(HINTS)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(status, area);
}
