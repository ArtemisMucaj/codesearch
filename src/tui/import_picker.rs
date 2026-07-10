//! Interactive session-import picker.
//!
//! A self-contained TUI screen (separate from the main tabbed app) shown when
//! `codesearch memory import` is run with no path. The left pane lists sessions
//! discovered from Claude Code / OpenCode / Zed — friendly name, how long ago,
//! and source — and the right pane shows the highlighted session's full
//! conversation, per turn, scrollable.
//!
//! Transcripts are **lazy-loaded** the first time a session is highlighted and
//! **cached**, so scrolling the list stays responsive and re-visiting a session
//! is instant. Returns the sessions the user checked; the caller runs them
//! through the import pipeline.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::domain::{DiscoveredSession, SessionMessage};
use crate::tui::widgets::markdown;

/// A function that materializes a discovered session's transcript on demand.
pub type TranscriptLoader<'a> =
    dyn Fn(&DiscoveredSession) -> Result<Vec<SessionMessage>, String> + 'a;

const SCROLL_STEP: u16 = 4;

/// Which pane has keyboard focus.
#[derive(PartialEq, Eq)]
enum Pane {
    List,
    Transcript,
}

/// The lazily-loaded transcript state for one session.
enum Loaded {
    Ok(Vec<SessionMessage>),
    Failed(String),
}

/// Run the picker to completion, returning the checked sessions (empty on
/// cancel). `load` materializes a session's transcript for the right pane.
pub fn run(
    sessions: Vec<DiscoveredSession>,
    now_secs: i64,
    load: &TranscriptLoader<'_>,
) -> Result<Vec<DiscoveredSession>> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, sessions, now_secs, load);
    ratatui::restore();
    result
}

struct PickerState {
    sessions: Vec<DiscoveredSession>,
    selected: usize,
    checked: HashSet<usize>,
    list_scroll: usize,
    transcript_scroll: u16,
    now_secs: i64,
    focus: Pane,
    /// Lazily-loaded, cached transcript per session index.
    cache: HashMap<usize, Loaded>,
    confirmed: bool,
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    sessions: Vec<DiscoveredSession>,
    now_secs: i64,
    load: &TranscriptLoader<'_>,
) -> Result<Vec<DiscoveredSession>> {
    let mut state = PickerState {
        sessions,
        selected: 0,
        checked: HashSet::new(),
        list_scroll: 0,
        transcript_scroll: 0,
        now_secs,
        focus: Pane::List,
        cache: HashMap::new(),
        confirmed: false,
    };

    loop {
        ensure_loaded(&mut state, load);
        terminal.draw(|f| render(f, &mut state))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Vec::new());
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                // Esc from the transcript returns to the list; from the list it quits.
                if state.focus == Pane::Transcript {
                    state.focus = Pane::List;
                } else {
                    return Ok(Vec::new());
                }
            }
            KeyCode::Tab => {
                state.focus = match state.focus {
                    Pane::List => Pane::Transcript,
                    Pane::Transcript => Pane::List,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => match state.focus {
                Pane::List => move_selection(&mut state, -1),
                Pane::Transcript => {
                    state.transcript_scroll = state.transcript_scroll.saturating_sub(SCROLL_STEP)
                }
            },
            KeyCode::Down | KeyCode::Char('j') => match state.focus {
                Pane::List => move_selection(&mut state, 1),
                Pane::Transcript => {
                    state.transcript_scroll = state.transcript_scroll.saturating_add(SCROLL_STEP)
                }
            },
            KeyCode::PageUp => {
                state.transcript_scroll = state.transcript_scroll.saturating_sub(SCROLL_STEP * 4)
            }
            KeyCode::PageDown => {
                state.transcript_scroll = state.transcript_scroll.saturating_add(SCROLL_STEP * 4)
            }
            KeyCode::Char(' ') => {
                if !state.checked.remove(&state.selected) {
                    state.checked.insert(state.selected);
                }
            }
            KeyCode::Char('a') => {
                if state.checked.len() == state.sessions.len() {
                    state.checked.clear();
                } else {
                    state.checked = (0..state.sessions.len()).collect();
                }
            }
            KeyCode::Enter => {
                state.confirmed = true;
                break;
            }
            _ => {}
        }
    }

    if !state.confirmed {
        return Ok(Vec::new());
    }
    let mut indices: Vec<usize> = if state.checked.is_empty() {
        vec![state.selected]
    } else {
        let mut v: Vec<usize> = state.checked.iter().copied().collect();
        v.sort_unstable();
        v
    };
    indices.dedup();
    Ok(indices
        .into_iter()
        .filter_map(|i| state.sessions.get(i).cloned())
        .collect())
}

/// Move the list cursor and reset the transcript scroll for the new session.
fn move_selection(state: &mut PickerState, delta: i32) {
    let len = state.sessions.len() as i32;
    let next = (state.selected as i32 + delta).clamp(0, len - 1) as usize;
    if next != state.selected {
        state.selected = next;
        state.transcript_scroll = 0;
    }
}

/// Load the highlighted session's transcript if it isn't cached yet.
fn ensure_loaded(state: &mut PickerState, load: &TranscriptLoader<'_>) {
    let idx = state.selected;
    if state.cache.contains_key(&idx) {
        return;
    }
    let loaded = match load(&state.sessions[idx]) {
        Ok(messages) => Loaded::Ok(messages),
        Err(e) => Loaded::Failed(e),
    };
    state.cache.insert(idx, loaded);
}

fn render(frame: &mut Frame, state: &mut PickerState) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(0),    // split panes
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    render_header(frame, rows[0], state);

    let panes =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(rows[1]);
    render_list(frame, panes[0], state);
    render_transcript(frame, panes[1], state);

    render_footer(frame, rows[2]);
}

fn render_header(frame: &mut Frame, area: Rect, state: &PickerState) {
    let text = format!(
        " Import sessions — {} found, {} selected",
        state.sessions.len(),
        state.checked.len()
    );
    frame.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut PickerState) {
    let focused = state.focus == Pane::List;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Sessions ")
        .border_style(Style::default().fg(if focused { Color::Cyan } else { Color::White }));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    if state.selected < state.list_scroll {
        state.list_scroll = state.selected;
    } else if height > 0 && state.selected >= state.list_scroll + height {
        state.list_scroll = state.selected + 1 - height;
    }

    let mut lines = Vec::new();
    for (i, s) in state
        .sessions
        .iter()
        .enumerate()
        .skip(state.list_scroll)
        .take(height)
    {
        let is_cursor = i == state.selected;
        let is_checked = state.checked.contains(&i);
        let checkbox = if is_checked { "[x]" } else { "[ ]" };
        let bg = if is_cursor {
            Color::DarkGray
        } else {
            Color::Reset
        };
        let source_color = match s.source.as_str() {
            "claude" => Color::Magenta,
            "opencode" => Color::Green,
            _ => Color::Blue,
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{checkbox} "),
                Style::default()
                    .fg(if is_checked {
                        Color::Green
                    } else {
                        Color::DarkGray
                    })
                    .bg(bg),
            ),
            Span::styled(
                format!("{:<8} ", s.source.as_str()),
                Style::default().fg(source_color).bg(bg),
            ),
            Span::styled(
                format!("{:>8}  ", relative_time(s.updated_at, state.now_secs)),
                Style::default().fg(Color::DarkGray).bg(bg),
            ),
            Span::styled(
                truncate(s.display_title(), inner.width.saturating_sub(22) as usize),
                Style::default()
                    .fg(if is_cursor { Color::White } else { Color::Gray })
                    .bg(bg)
                    .add_modifier(if is_cursor {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &PickerState) {
    let focused = state.focus == Pane::Transcript;
    let title = state
        .sessions
        .get(state.selected)
        .map(|s| format!(" {} ", truncate(s.display_title(), 50)))
        .unwrap_or_else(|| " Transcript ".to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused { Color::Cyan } else { Color::White }));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = match state.cache.get(&state.selected) {
        Some(Loaded::Ok(messages)) => render_conversation(messages),
        Some(Loaded::Failed(e)) => vec![Line::from(Span::styled(
            format!("Could not load transcript: {e}"),
            Style::default().fg(Color::Red),
        ))],
        None => vec![Line::from(Span::styled(
            "Loading…",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((state.transcript_scroll, 0)),
        inner,
    );
}

/// Render a per-turn conversation: a coloured role header per message followed
/// by its Markdown-rendered content, with a blank line between turns.
fn render_conversation(messages: &[SessionMessage]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for msg in messages {
        if msg.content.trim().is_empty() {
            continue;
        }
        let (label, color) = match msg.role.as_str() {
            "user" => ("▌ User", Color::Cyan),
            "assistant" => ("▌ Assistant", Color::Green),
            other => (role_static(other), Color::Yellow),
        };
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        lines.extend(markdown::render(&msg.content));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no textual content)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

/// A stable role label for non-user/assistant roles.
fn role_static(role: &str) -> &'static str {
    match role {
        "system" => "▌ System",
        "tool" => "▌ Tool",
        _ => "▌ Message",
    }
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let hint = " ↑↓/jk: move  Space: toggle  a: all  Tab: focus transcript  \
                PgUp/Dn: scroll  Enter: import  Esc/Ctrl+C: cancel";
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

/// A compact "N ago" label from two Unix timestamps.
fn relative_time(then_secs: i64, now_secs: i64) -> String {
    let d = (now_secs - then_secs).max(0);
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else if d < 86400 * 30 {
        format!("{}d ago", d / 86400)
    } else if d < 86400 * 365 {
        format!("{}mo ago", d / (86400 * 30))
    } else {
        format!("{}y ago", d / (86400 * 365))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: None,
        }
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000;
        assert_eq!(relative_time(now - 30, now), "just now");
        assert_eq!(relative_time(now - 120, now), "2m ago");
        assert_eq!(relative_time(now - 7200, now), "2h ago");
        assert_eq!(relative_time(now - 86400 * 3, now), "3d ago");
    }

    #[test]
    fn conversation_has_role_headers_per_turn() {
        let msgs = vec![msg("user", "hi"), msg("assistant", "hello back")];
        let lines = render_conversation(&msgs);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("User"));
        assert!(text.contains("Assistant"));
        assert!(text.contains("hello back"));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert!(truncate("hello world", 5).ends_with('…'));
    }
}
