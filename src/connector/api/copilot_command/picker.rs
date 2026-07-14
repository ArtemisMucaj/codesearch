//! Full-screen ratatui model picker shown by `codesearch copilot login`.
//!
//! One screen: a scrollable list of the account's Copilot models. `↑/↓` (or
//! `j/k`) moves the cursor, `Enter` selects, `Esc`/`q` cancels. The right pane
//! shows the highlighted model's capabilities so the choice is informed.
//!
//! Runs synchronously on a blocking thread (see the caller): it owns the
//! terminal via `ratatui::init()` / `ratatui::restore()` and returns the chosen
//! index (or `None` if cancelled).

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::connector::adapter::CopilotModel as Model;

/// Run the picker over `models`, starting the cursor at `preselected`.
///
/// Returns `Ok(Some(index))` for the chosen model, `Ok(None)` if the user
/// cancelled. `models` must be non-empty (the caller guarantees this).
pub fn run(models: &[Model], preselected: usize) -> Result<Option<usize>> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, models, preselected);
    ratatui::restore();
    result
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    models: &[Model],
    preselected: usize,
) -> Result<Option<usize>> {
    let mut state = ListState::default();
    state.select(Some(preselected.min(models.len().saturating_sub(1))));

    loop {
        terminal.draw(|frame| draw(frame, models, &mut state))?;

        // Blocking read is fine — this screen is purely input-driven with no
        // background updates to poll for.
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => move_cursor(&mut state, models.len(), -1),
            KeyCode::Down | KeyCode::Char('j') => move_cursor(&mut state, models.len(), 1),
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

fn draw(frame: &mut Frame, models: &[Model], state: &mut ListState) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);

    let items: Vec<ListItem> = models
        .iter()
        .map(|m| ListItem::new(Line::from(m.id.clone())))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Copilot models "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, list_area, state);

    let selected = state.selected().and_then(|i| models.get(i));
    frame.render_widget(detail_panel(selected), detail_area);

    let footer_text = Line::from(" ↑/↓ or j/k: move   Enter: select   Esc/q: cancel ").style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(footer_text), footer);
}

/// Build the right-hand detail pane for the highlighted model.
fn detail_panel(model: Option<&Model>) -> Paragraph<'static> {
    let block = Block::default().borders(Borders::ALL).title(" Details ");
    let Some(m) = model else {
        return Paragraph::new("").block(block);
    };

    let mut lines: Vec<Line<'static>> = vec![field("Name", &m.name), field("ID", &m.id)];
    if let Some(vendor) = &m.vendor {
        lines.push(field("Vendor", vendor));
    }
    if let Some(limits) = m.capabilities.as_ref().and_then(|c| c.limits.as_ref()) {
        if let Some(ctx) = limits.max_context_window_tokens {
            lines.push(field("Context window", &format!("{ctx} tokens")));
        }
        if let Some(out) = limits.max_output_tokens {
            lines.push(field("Max output", &format!("{out} tokens")));
        }
    }
    if m.preview {
        lines.push(Line::from(Span::styled(
            "preview",
            Style::default().fg(Color::Yellow),
        )));
    }

    Paragraph::new(lines).block(block).wrap(Wrap { trim: true })
}

/// A `label: value` detail line with a dim label.
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::raw(value.to_string()),
    ])
}
