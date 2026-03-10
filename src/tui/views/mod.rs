pub(crate) mod context;
mod format;
mod impact;
mod search;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::state::{ActiveMode, AppState, ContextPane, ImpactPane, SearchPane};
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

    let hints: &str = match state.mode {
        ActiveMode::Search => match state.search.focused_pane {
            SearchPane::List => {
                " Enter: search  ↑↓: navigate  ←→: cursor  Ctrl+→: code  Ctrl+↑: impact  Ctrl+↓: context  Tab: switch  q: quit"
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
        ActiveMode::Context => match state.context.focused_pane {
            ContextPane::EntryPoints => {
                " Enter: analyse  ↑↓: navigate  ←→: cursor  Ctrl+→: focus tree  Tab: switch  q: quit"
            }
            ContextPane::Tree => {
                if state.context.chain_snippet.is_some() {
                    " ↑↓/PgUp/Dn: scroll  Esc: back to tree  q: quit"
                } else {
                    " ↑↓: navigate nodes  Enter: view code  PgUp/Dn: scroll fast  Ctrl+←: focus list  Tab: switch  q: quit"
                }
            }
        },
    };

    let status = Paragraph::new(hints)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(status, area);
}
