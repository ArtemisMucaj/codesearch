use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::state::{AppState, ContextPane};
use crate::tui::widgets::code_panel;
use crate::tui::widgets::result_list::ListEntry;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panes = Layout::horizontal([
        Constraint::Percentage(35),
        Constraint::Percentage(65),
    ])
    .split(area);

    render_edges(frame, panes[0], state);
    render_snippet(frame, panes[1], state);
}

fn render_edges(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;

    if let Some(err) = &s.error {
        let block = Block::default()
            .title(" Context ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        frame.render_widget(
            Paragraph::new(format!("Error: {}", err)).block(block),
            area,
        );
        return;
    }

    // Split the left pane vertically: callers on top, callees on bottom.
    let halves = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .split(area);

    let callers_focused = s.focused_pane == ContextPane::Callers;

    // ── Callers ──────────────────────────────────────────────────────────────
    let caller_entries: Vec<ListEntry> = match &s.context {
        None => vec![],
        Some(ctx) => ctx
            .callers
            .iter()
            .map(|e| ListEntry {
                label: format!("{}:{}", shorten_path(&e.file_path), e.line),
                sub_label: Some(short_symbol(&e.symbol).to_string()),
                score: None,
            })
            .collect(),
    };

    let callers_border = if callers_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // render_stateful_widget wants ownership of state, so we inline the block title.
    let caller_count = caller_entries.len();
    let _ = callers_border; // used below via result_list which sets its own border

    render_pane_list(
        frame,
        halves[0],
        &format!("Callers ({})", caller_count),
        &caller_entries,
        s.selected_caller,
        callers_focused,
    );

    // ── Callees ───────────────────────────────────────────────────────────────
    let callee_entries: Vec<ListEntry> = match &s.context {
        None => vec![],
        Some(ctx) => ctx
            .callees
            .iter()
            .map(|e| ListEntry {
                label: format!("{}:{}", shorten_path(&e.file_path), e.line),
                sub_label: Some(short_symbol(&e.symbol).to_string()),
                score: None,
            })
            .collect(),
    };

    let callee_count = callee_entries.len();

    render_pane_list(
        frame,
        halves[1],
        &format!("Callees ({})", callee_count),
        &callee_entries,
        s.selected_callee,
        !callers_focused,
    );
}

fn render_pane_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[ListEntry],
    selected: usize,
    focused: bool,
) {
    use ratatui::widgets::{List, ListItem, ListState};
    use ratatui::text::{Line, Span};

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let is_sel = i == selected && focused;
            let bullet = if is_sel { "●" } else { "○" };
            let label_style = if is_sel {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{} ", bullet), label_style),
                Span::styled(e.label.clone(), label_style),
            ])];
            if let Some(sub) = &e.sub_label {
                lines.push(Line::from(Span::styled(
                    format!("   {}", sub),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(format!(" {} ", title))
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    let mut list_state = ListState::default();
    if focused && !entries.is_empty() {
        list_state.select(Some(selected));
    }

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_snippet(frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.context;

    let (content, title, start_line) = match &s.snippet {
        Some(chunk) => (
            chunk.content().to_string(),
            shorten_path(chunk.file_path()),
            chunk.start_line(),
        ),
        None => {
            let hint = if s.snippet_loading {
                "  Loading snippet…"
            } else if s.context.is_some() {
                "  Select a caller or callee to view its code."
            } else {
                "  Enter a symbol and press Enter."
            };
            let block = Block::default()
                .title(" Code ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                Paragraph::new(hint)
                    .block(block)
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
    };

    code_panel::render(frame, area, &title, &content, start_line, s.snippet_scroll);
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

fn short_symbol(fq: &str) -> &str {
    fq.rsplit(&['/', ':', '.']).next().unwrap_or(fq)
}
