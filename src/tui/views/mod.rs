mod format;
mod impact;
mod search;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::state::{ActiveMode, AppState, ImpactPane, SearchPane};
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
    }

    render_status(frame, areas[2], state);
}

fn render_status(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let hints: &str = match state.mode {
        ActiveMode::Search => match state.search.focused_pane {
            SearchPane::List => {
                " Enter: search  ↑↓: navigate  ←→: cursor  Ctrl+→: focus code  Ctrl+↑: impact  Tab: switch  q: quit"
            }
            SearchPane::Code => {
                " Ctrl+←: focus list  ↑↓/PgUp/Dn: scroll  ←→: cursor  Tab: switch  q: quit"
            }
        },
        ActiveMode::Impact => match state.impact.focused_pane {
            ImpactPane::EntryPoints => {
                " Enter: analyse  ↑↓: navigate  ←→: cursor  Ctrl+→: focus chain  Tab: switch  q: quit"
            }
            ImpactPane::Chain => {
                if state.impact.chain_snippet.is_some() {
                    " ↑↓/PgUp/Dn: scroll  Esc: back to chain  q: quit"
                } else {
                    " ↑↓: select node  Enter: view code  Ctrl+←: focus list  Tab: switch  q: quit"
                }
            }
        },
    };

    let status = Paragraph::new(hints)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(status, area);
}
