//! Minimal ratatui picker over a list of model ids, shown by
//! `codesearch openai select`.
//!
//! `↑/↓` (or `j/k`) moves the cursor, `Enter` selects, `Esc`/`q` cancels. Runs
//! synchronously on a blocking thread; owns the terminal via `ratatui::init()` /
//! `ratatui::restore()` and returns the chosen index (or `None` if cancelled).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

/// Run the picker over `ids`, starting the cursor at `preselected`. Returns the
/// chosen index, or `None` if cancelled. `ids` must be non-empty.
pub fn run(ids: &[String], preselected: usize) -> Result<Option<usize>> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, ids, preselected);
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    ids: &[String],
    preselected: usize,
) -> Result<Option<usize>> {
    let mut state = ListState::default();
    state.select(Some(preselected.min(ids.len().saturating_sub(1))));

    loop {
        terminal.draw(|frame| draw(frame, ids, &mut state))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => move_cursor(&mut state, ids.len(), -1),
            KeyCode::Down | KeyCode::Char('j') => move_cursor(&mut state, ids.len(), 1),
            KeyCode::Enter => return Ok(state.selected()),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

/// Move the list cursor by `delta`, clamping to `[0, len)`.
fn move_cursor(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, len as isize - 1);
    state.select(Some(next as usize));
}

fn draw(frame: &mut Frame, ids: &[String], state: &mut ListState) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    let items: Vec<ListItem> = ids
        .iter()
        .map(|id| ListItem::new(Line::from(id.clone())))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Models "))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, body, state);

    let footer_text = Line::from(" ↑/↓ or j/k: move   Enter: select   Esc/q: cancel ").style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(footer_text), footer);
}
