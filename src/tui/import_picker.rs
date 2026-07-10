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
//! is instant. Pressing `i` imports the highlighted session: the request is
//! sent to a background worker that runs extraction and reports progress back,
//! so the picker stays open and each row shows its live status (queued →
//! importing → ✓). Already-imported sessions are marked ✓ on open.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

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

/// How long the input loop waits for a key before checking for newly-discovered
/// sessions and redrawing. Short enough that streamed sessions appear promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Stable identity of a discovered session: `(source, id)`. Used as the key for
/// import status and for pinning selection across re-sorts.
pub type SessionId = (String, String);

/// A request from the picker to the background worker: import this session.
/// The worker already holds the container; it materializes the transcript,
/// runs extraction, and reports back via [`ImportEvent`].
pub struct ImportRequest {
    pub session: DiscoveredSession,
}

/// A status update from the background import worker to the picker.
pub enum ImportEvent {
    /// The container finished loading; imports are now available. Carries the
    /// set of sessions already present in the memory store (for the ✓ marks).
    Ready { imported: HashSet<SessionId> },
    /// The container failed to build; imports are unavailable this run.
    ContainerFailed { error: String },
    /// Extraction started for a session.
    Started { id: SessionId },
    /// Extraction finished; `summary` is a one-line outcome for the footer.
    Done { id: SessionId, summary: String },
    /// Extraction failed for a session.
    Failed { id: SessionId, error: String },
}

/// Import lifecycle of a single session, shown as the list's left-hand marker.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ImportStatus {
    /// Not yet imported and not queued.
    None,
    /// Already present in the memory store when the picker opened.
    AlreadyImported,
    /// Sent to the worker, extraction not yet started.
    Queued,
    /// Extraction in progress.
    Importing,
    /// Extraction finished this session (freshly imported or re-imported).
    Done,
    /// Extraction failed; carries a short reason for the footer.
    Failed(String),
}

/// Whether the background container/worker is available for imports yet.
enum WorkerState {
    /// Models still loading; `i` shows "loading…" instead of importing.
    Loading,
    Ready,
    Failed(String),
}

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

/// Run the picker to completion. Sessions arrive on `incoming` as each discovery
/// source reports, so the picker opens instantly and fills in. The user imports
/// the highlighted session with `i`: the request goes to the background worker
/// over `import_tx`, and progress comes back on `events` — so imports run
/// without closing the picker. `load` materializes a session's transcript for
/// the right pane.
///
/// Returns when the user quits; the return value is unused (imports are applied
/// by the worker as they happen), but kept as `Result` to surface terminal I/O
/// errors.
pub fn run(
    incoming: Receiver<Vec<DiscoveredSession>>,
    events: Receiver<ImportEvent>,
    import_tx: Sender<ImportRequest>,
    now_secs: i64,
    load: &TranscriptLoader<'_>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, incoming, events, import_tx, now_secs, load);
    ratatui::restore();
    result
}

struct PickerState {
    sessions: Vec<DiscoveredSession>,
    selected: usize,
    list_scroll: usize,
    transcript_scroll: u16,
    now_secs: i64,
    focus: Pane,
    /// Lazily-loaded, cached transcript per session index.
    cache: HashMap<usize, Loaded>,
    /// True while at least one discovery source is still reporting.
    discovering: bool,
    /// Import status keyed by session identity, so it survives list re-sorts.
    status: HashMap<SessionId, ImportStatus>,
    /// Whether the background container/worker can service imports yet.
    worker: WorkerState,
    /// One-line result of the most recent import, shown in the footer.
    last_result: Option<String>,
}

impl PickerState {
    /// Import status of the session at list index `idx` (defaults to `None`).
    #[cfg(test)]
    fn status_at(&self, idx: usize) -> ImportStatus {
        self.sessions
            .get(idx)
            .and_then(|s| self.status.get(&session_key(s)))
            .cloned()
            .unwrap_or(ImportStatus::None)
    }
}

/// Drain any newly-discovered session batches into state, keeping the list
/// sorted newest-first and the highlighted session pinned across the re-sort.
/// Returns whether the list changed (so we know to redraw). Clears
/// `discovering` once the sender side has hung up. Import status lives in a
/// map keyed by session identity, so it follows sessions across the re-sort
/// with no reindexing.
fn drain_incoming(state: &mut PickerState, incoming: &Receiver<Vec<DiscoveredSession>>) -> bool {
    let selected_id = state.sessions.get(state.selected).map(session_key);

    let mut changed = false;
    loop {
        match incoming.try_recv() {
            Ok(batch) => {
                state.sessions.extend(batch);
                changed = true;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if state.discovering {
                    state.discovering = false;
                    changed = true;
                }
                break;
            }
        }
    }

    if changed {
        state
            .sessions
            .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        // The transcript cache is keyed by index, which the re-sort invalidates;
        // clear it and let the highlighted session reload lazily next tick.
        state.cache.clear();
        // Pin the cursor to the same session it was on before the re-sort.
        if let Some(sel) = selected_id {
            if let Some(i) = state.sessions.iter().position(|s| session_key(s) == sel) {
                state.selected = i;
            }
        }
        state.selected = state.selected.min(state.sessions.len().saturating_sub(1));
    }
    changed
}

/// Drain import worker events into state. Returns whether anything changed.
fn drain_events(state: &mut PickerState, events: &Receiver<ImportEvent>) -> bool {
    let mut changed = false;
    loop {
        match events.try_recv() {
            Ok(ImportEvent::Ready { imported }) => {
                state.worker = WorkerState::Ready;
                for id in imported {
                    // Don't clobber a status set by an in-flight import.
                    state
                        .status
                        .entry(id)
                        .or_insert(ImportStatus::AlreadyImported);
                }
                changed = true;
            }
            Ok(ImportEvent::ContainerFailed { error }) => {
                state.worker = WorkerState::Failed(error);
                changed = true;
            }
            Ok(ImportEvent::Started { id }) => {
                state.status.insert(id, ImportStatus::Importing);
                changed = true;
            }
            Ok(ImportEvent::Done { id, summary }) => {
                state.status.insert(id, ImportStatus::Done);
                state.last_result = Some(summary);
                changed = true;
            }
            Ok(ImportEvent::Failed { id, error }) => {
                state.status.insert(id, ImportStatus::Failed(error.clone()));
                state.last_result = Some(format!("Import failed: {error}"));
                changed = true;
            }
            // The worker hung up (container-build task ended). Nothing more to
            // do; leave existing state as-is.
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    changed
}

/// Queue the highlighted session for import, sending it to the worker. No-op
/// when the worker isn't ready, the session is missing, or it is already
/// importing/queued.
fn import_selected(state: &mut PickerState, import_tx: &Sender<ImportRequest>) {
    if !matches!(state.worker, WorkerState::Ready) {
        return;
    }
    let Some(session) = state.sessions.get(state.selected).cloned() else {
        return;
    };
    let key = session_key(&session);
    // Don't double-queue an in-flight import; re-importing a Done/Already one is
    // allowed (the worker force-re-runs extraction).
    if matches!(
        state.status.get(&key),
        Some(ImportStatus::Queued | ImportStatus::Importing)
    ) {
        return;
    }
    state.status.insert(key, ImportStatus::Queued);
    // A send error means the worker is gone; reflect that in the footer.
    if import_tx.send(ImportRequest { session }).is_err() {
        state.worker = WorkerState::Failed("import worker stopped".to_string());
    }
}

/// Stable identity for a discovered session, used to key import status and to
/// keep the cursor pinned across re-sorts as new sessions stream in.
fn session_key(s: &DiscoveredSession) -> SessionId {
    (s.source.as_str().to_string(), s.id.clone())
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    incoming: Receiver<Vec<DiscoveredSession>>,
    events: Receiver<ImportEvent>,
    import_tx: Sender<ImportRequest>,
    now_secs: i64,
    load: &TranscriptLoader<'_>,
) -> Result<()> {
    let mut state = PickerState {
        sessions: Vec::new(),
        selected: 0,
        list_scroll: 0,
        transcript_scroll: 0,
        now_secs,
        focus: Pane::List,
        cache: HashMap::new(),
        discovering: true,
        status: HashMap::new(),
        worker: WorkerState::Loading,
        last_result: None,
    };

    loop {
        drain_incoming(&mut state, &incoming);
        drain_events(&mut state, &events);
        ensure_loaded(&mut state, load);
        terminal.draw(|f| render(f, &mut state))?;

        // Poll so streamed sessions, import progress, and the "discovering"
        // state stay live even when the user isn't pressing keys.
        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(());
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                // Esc from the transcript returns to the list; from the list it quits.
                if state.focus == Pane::Transcript {
                    state.focus = Pane::List;
                } else {
                    return Ok(());
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
            // Import the highlighted session (Enter or `i`); the picker stays
            // open and the row shows a spinner → ✓ as the worker reports back.
            KeyCode::Enter | KeyCode::Char('i') => {
                import_selected(&mut state, &import_tx);
            }
            _ => {}
        }
    }
}

/// Move the list cursor and reset the transcript scroll for the new session.
fn move_selection(state: &mut PickerState, delta: i32) {
    if state.sessions.is_empty() {
        return;
    }
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
    let Some(session) = state.sessions.get(idx) else {
        return; // No sessions discovered yet.
    };
    let loaded = match load(session) {
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

    render_footer(frame, rows[2], state);
}

fn render_header(frame: &mut Frame, area: Rect, state: &PickerState) {
    let imported = state
        .status
        .values()
        .filter(|s| matches!(s, ImportStatus::Done | ImportStatus::AlreadyImported))
        .count();

    let mut notes = String::new();
    if state.discovering {
        notes.push_str(" · discovering…");
    }
    match &state.worker {
        WorkerState::Loading => notes.push_str(" · loading models…"),
        WorkerState::Failed(e) => notes.push_str(&format!(" · import unavailable ({e})")),
        WorkerState::Ready => {}
    }

    let text = format!(
        " Import sessions — {} found, {} imported{}",
        state.sessions.len(),
        imported,
        notes
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

/// The list-row marker for a session's import status: glyph + colour.
fn status_marker(status: &ImportStatus) -> (&'static str, Color) {
    match status {
        ImportStatus::None => ("[ ]", Color::DarkGray),
        ImportStatus::AlreadyImported => ("[✓]", Color::Green),
        ImportStatus::Queued => ("[…]", Color::Yellow),
        ImportStatus::Importing => ("[⟳]", Color::Cyan),
        ImportStatus::Done => ("[✓]", Color::Green),
        ImportStatus::Failed(_) => ("[✗]", Color::Red),
    }
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
        let status = state
            .status
            .get(&session_key(s))
            .cloned()
            .unwrap_or(ImportStatus::None);
        let (marker, marker_color) = status_marker(&status);
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
                format!("{marker} "),
                Style::default().fg(marker_color).bg(bg),
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

    let lines: Vec<Line> = if state.sessions.is_empty() {
        let msg = if state.discovering {
            "Discovering sessions…"
        } else {
            "No sessions found."
        };
        vec![Line::from(Span::styled(
            msg,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        match state.cache.get(&state.selected) {
            Some(Loaded::Ok(messages)) => render_conversation(messages),
            Some(Loaded::Failed(e)) => vec![Line::from(Span::styled(
                format!("Could not load transcript: {e}"),
                Style::default().fg(Color::Red),
            ))],
            None => vec![Line::from(Span::styled(
                "Loading…",
                Style::default().fg(Color::DarkGray),
            ))],
        }
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

fn render_footer(frame: &mut Frame, area: Rect, state: &PickerState) {
    // Prefer showing the most recent import result; fall back to key hints.
    let (text, color) = match &state.last_result {
        Some(msg) => (format!(" {msg}"), Color::Green),
        None => (
            " ↑↓/jk: move  i/Enter: import highlighted  Tab: focus transcript  \
             PgUp/Dn: scroll  Esc/Ctrl+C: quit"
                .to_string(),
            Color::DarkGray,
        ),
    };
    frame.render_widget(Paragraph::new(text).style(Style::default().fg(color)), area);
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

    fn session(id: &str, updated_at: i64) -> DiscoveredSession {
        DiscoveredSession {
            source: crate::domain::SessionSource::Claude,
            id: id.to_string(),
            title: id.to_string(),
            cwd: None,
            updated_at,
            message_count: 1,
            tail_preview: String::new(),
            locator: crate::domain::SessionLocator::File(format!("{id}.jsonl")),
        }
    }

    fn empty_state() -> PickerState {
        PickerState {
            sessions: Vec::new(),
            selected: 0,
            list_scroll: 0,
            transcript_scroll: 0,
            now_secs: 0,
            focus: Pane::List,
            cache: HashMap::new(),
            discovering: true,
            status: HashMap::new(),
            worker: WorkerState::Loading,
            last_result: None,
        }
    }

    #[test]
    fn drain_merges_and_sorts_newest_first() {
        let mut state = empty_state();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(vec![session("old", 100), session("new", 300)])
            .unwrap();
        tx.send(vec![session("mid", 200)]).unwrap();

        assert!(drain_incoming(&mut state, &rx));
        let order: Vec<&str> = state.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(order, ["new", "mid", "old"]);
        // Sender still alive, so discovery is ongoing.
        assert!(state.discovering);
    }

    #[test]
    fn drain_preserves_cursor_and_status_across_resort() {
        let mut state = empty_state();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(vec![session("a", 100)]).unwrap();
        drain_incoming(&mut state, &rx);
        // Mark "a" imported and highlight it.
        state
            .status
            .insert(session_key(&session("a", 100)), ImportStatus::Done);
        state.selected = 0;

        // A newer session arrives and re-sorts "a" to the back.
        tx.send(vec![session("b", 999)]).unwrap();
        assert!(drain_incoming(&mut state, &rx));

        let a_idx = state.sessions.iter().position(|s| s.id == "a").unwrap();
        assert_eq!(state.sessions[0].id, "b"); // newest first
        assert_eq!(state.selected, a_idx); // cursor followed the session
                                           // Status is keyed by identity, so it still applies to "a".
        assert_eq!(state.status_at(a_idx), ImportStatus::Done);
    }

    #[test]
    fn drain_clears_discovering_when_sender_hangs_up() {
        let mut state = empty_state();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<DiscoveredSession>>();
        drop(tx);
        // Disconnected with nothing sent still counts as a change (flips the
        // "discovering" flag off so the header stops showing the spinner).
        assert!(drain_incoming(&mut state, &rx));
        assert!(!state.discovering);
    }

    #[test]
    fn drain_reports_no_change_when_empty_and_still_connected() {
        let mut state = empty_state();
        let (_tx, rx) = std::sync::mpsc::channel::<Vec<DiscoveredSession>>();
        assert!(!drain_incoming(&mut state, &rx));
        assert!(state.discovering);
    }

    #[test]
    fn ready_event_marks_already_imported() {
        let mut state = empty_state();
        // "a" is in the store; "b" is not.
        state.sessions = vec![session("a", 100), session("b", 200)];
        let (tx, rx) = std::sync::mpsc::channel();
        let imported: HashSet<SessionId> = [session_key(&session("a", 0))].into_iter().collect();
        tx.send(ImportEvent::Ready { imported }).unwrap();

        assert!(drain_events(&mut state, &rx));
        assert!(matches!(state.worker, WorkerState::Ready));
        assert_eq!(state.status_at(0), ImportStatus::AlreadyImported); // a
        assert_eq!(state.status_at(1), ImportStatus::None); // b
    }

    #[test]
    fn import_events_drive_status_transitions() {
        let mut state = empty_state();
        state.sessions = vec![session("a", 100)];
        state.worker = WorkerState::Ready;
        let id = session_key(&session("a", 0));
        let (tx, rx) = std::sync::mpsc::channel();

        tx.send(ImportEvent::Started { id: id.clone() }).unwrap();
        drain_events(&mut state, &rx);
        assert_eq!(state.status_at(0), ImportStatus::Importing);

        tx.send(ImportEvent::Done {
            id: id.clone(),
            summary: "1 memory written".to_string(),
        })
        .unwrap();
        drain_events(&mut state, &rx);
        assert_eq!(state.status_at(0), ImportStatus::Done);
        assert_eq!(state.last_result.as_deref(), Some("1 memory written"));
    }

    #[test]
    fn import_selected_queues_only_when_worker_ready() {
        let mut state = empty_state();
        state.sessions = vec![session("a", 100)];
        let (tx, rx) = std::sync::mpsc::channel();

        // Worker still loading — nothing is queued or sent.
        import_selected(&mut state, &tx);
        assert_eq!(state.status_at(0), ImportStatus::None);
        assert!(rx.try_recv().is_err());

        // Worker ready — the highlighted session is queued and sent.
        state.worker = WorkerState::Ready;
        import_selected(&mut state, &tx);
        assert_eq!(state.status_at(0), ImportStatus::Queued);
        assert!(rx.try_recv().is_ok());

        // A second press while queued does not double-send.
        import_selected(&mut state, &tx);
        assert!(rx.try_recv().is_err());
    }
}
