use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::application::{
    ImpactAnalysisUseCase, MemoryBrowseUseCase, SearchCodeUseCase, SnippetLookupUseCase,
    SymbolContextUseCase,
};
use crate::domain::SearchQuery;

use super::cache::TuiCache;
use super::event::TuiEvent;
use super::state::{ActiveMode, AppState, ContextPane, ImpactPane, MemoryPane, SearchPane};
use super::views;
use super::views::context::{build_flat_tree_for_selected, leaf_caller_nodes};
use crate::cli::TuiMode;

const SEARCH_LIMIT: usize = 20;
const SCROLL_STEP: u16 = 5;
/// Maximum entries returned by a memory search/browse.
const MEMORY_LIMIT: usize = 50;

pub struct TuiApp {
    state: AppState,
    cache: TuiCache,
    /// `None` while the background container task is still running.
    search_uc: Option<Arc<SearchCodeUseCase>>,
    /// `None` while the background container task is still running.
    impact_uc: Option<Arc<ImpactAnalysisUseCase>>,
    /// `None` while the background container task is still running.
    snippet_uc: Option<Arc<SnippetLookupUseCase>>,
    /// `None` while the background container task is still running.
    context_uc: Option<Arc<SymbolContextUseCase>>,
    /// `None` while the background container task is still running.
    memory_uc: Option<Arc<MemoryBrowseUseCase>>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    impact_task: Option<tokio::task::JoinHandle<()>>,
    context_task: Option<tokio::task::JoinHandle<()>>,
    memory_task: Option<tokio::task::JoinHandle<()>>,
}

impl TuiApp {
    /// Create a TUI app that waits for the container to finish loading in the
    /// background.  The caller spawns `Container::new()` and sends the result
    /// via `container_tx`; `TuiApp` shows a status-bar hint while waiting and
    /// enables dispatching once the event arrives.
    pub fn new_loading(
        repository: Option<String>,
        mode: TuiMode,
        query: Option<String>,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
        event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    ) -> Self {
        let initial_mode = match mode {
            TuiMode::Search => ActiveMode::Search,
            TuiMode::Impact => ActiveMode::Impact,
            TuiMode::Context => ActiveMode::Context,
        };
        Self {
            state: AppState::new(repository, initial_mode, query, false),
            cache: TuiCache::default(),
            search_uc: None,
            impact_uc: None,
            snippet_uc: None,
            context_uc: None,
            memory_uc: None,
            event_tx,
            event_rx,
            impact_task: None,
            context_task: None,
            memory_task: None,
        }
    }

    /// Create a TUI app when use-cases are already available (models loaded).
    pub fn new(
        search_uc: Arc<SearchCodeUseCase>,
        impact_uc: Arc<ImpactAnalysisUseCase>,
        snippet_uc: Arc<SnippetLookupUseCase>,
        repository: Option<String>,
        mode: TuiMode,
        query: Option<String>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let initial_mode = match mode {
            TuiMode::Search => ActiveMode::Search,
            TuiMode::Impact => ActiveMode::Impact,
            TuiMode::Context => ActiveMode::Context,
        };
        Self {
            state: AppState::new(repository, initial_mode, query, true),
            cache: TuiCache::default(),
            search_uc: Some(search_uc),
            impact_uc: Some(impact_uc),
            snippet_uc: Some(snippet_uc),
            context_uc: None,
            memory_uc: None,
            event_tx: tx,
            event_rx: rx,
            impact_task: None,
            context_task: None,
            memory_task: None,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        let result = self.run_with_terminal(&mut terminal).await;
        ratatui::restore();
        result
    }

    /// Run the TUI using an already-initialised terminal.
    ///
    /// Use this when the caller has already called `ratatui::init()` (e.g. to
    /// show a loading splash before the container is built).  The caller is
    /// responsible for calling `ratatui::restore()` after this returns.
    pub async fn run_with_terminal(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
    ) -> Result<()> {
        // Auto-dispatch the initial query only when models are already ready.
        // If we are in the lazy-loading path the auto-dispatch is triggered
        // from `handle_app_event` once `ContainerReady` arrives.
        if self.state.models_ready {
            match self.state.mode {
                ActiveMode::Search if !self.state.search.input.is_empty() => {
                    self.dispatch_search();
                }
                ActiveMode::Impact if !self.state.impact.input.is_empty() => {
                    self.dispatch_impact();
                }
                ActiveMode::Context if !self.state.context.input.is_empty() => {
                    self.dispatch_context();
                }
                _ => {}
            }
        }
        self.run_loop(terminal).await
    }

    async fn run_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut stream = EventStream::new();

        loop {
            terminal.draw(|f| views::render(f, &self.state))?;

            tokio::select! {
                Some(app_ev) = self.event_rx.recv() => {
                    self.handle_app_event(app_ev);
                }
                maybe_ev = stream.next() => {
                    match maybe_ev {
                        Some(Ok(Event::Key(key))) => self.handle_key(key),
                        Some(Ok(Event::Resize(..))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            warn!("terminal event error: {}", e);
                            break;
                        }
                        None => break,
                    }
                }
            }

            if self.state.should_quit {
                break;
            }
        }

        Ok(())
    }

    // ── Keyboard handling ─────────────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // `q` quits from a focused detail pane in the code-navigation modes.
            // Memory's detail pane shows scrollable prose (transcripts), where a
            // stray `q` quitting mid-read is a footgun — there, use Ctrl+C.
            KeyCode::Char('q')
                if key.modifiers == KeyModifiers::NONE
                    && self.state.mode != ActiveMode::Memory
                    && self.state.detail_pane_focused() =>
            {
                self.state.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.should_quit = true;
            }
            KeyCode::Esc => self.handle_esc(),
            // Tab cycles modes forward; Shift+Tab (BackTab) cycles backward.
            KeyCode::Tab => {
                self.cycle_mode(1);
            }
            KeyCode::BackTab => {
                self.cycle_mode(-1);
            }
            // Shift+i / Shift+x (capital I / X) jump the selected symbol to
            // Impact / Context. Active on any non-input (right/detail) pane, so
            // they never collide with typing a capitalized query — in a
            // query-input pane the letters fall through and type normally. Plain
            // Shift only, reliable on macOS unlike Ctrl/Alt.
            KeyCode::Char('I') if self.state.detail_pane_focused() => {
                self.jump_to_impact();
            }
            KeyCode::Char('X') if self.state.detail_pane_focused() => {
                self.jump_to_context();
            }
            // Enter drills into the focused pane (search → code, analyze, load
            // snippet); Esc backs out one level.
            KeyCode::Enter => self.dispatch_current(),
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.navigate(-1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.navigate(1),
            KeyCode::Left if key.modifiers == KeyModifiers::NONE => {
                let c = self.state.active_cursor_mut();
                *c = c.saturating_sub(1);
            }
            KeyCode::Right if key.modifiers == KeyModifiers::NONE => {
                let len = self.state.active_input().chars().count();
                let c = self.state.active_cursor_mut();
                if *c < len {
                    *c += 1;
                }
            }
            KeyCode::Home => {
                *self.state.active_cursor_mut() = 0;
            }
            KeyCode::End => {
                let len = self.state.active_input().chars().count();
                *self.state.active_cursor_mut() = len;
            }
            KeyCode::PageUp => self.scroll_code(-(SCROLL_STEP as i32)),
            KeyCode::PageDown => self.scroll_code(SCROLL_STEP as i32),
            KeyCode::Backspace => {
                let cursor = self.state.active_cursor();
                if cursor > 0 {
                    let byte_idx = {
                        let input = self.state.active_input_mut();
                        let idx = input
                            .char_indices()
                            .nth(cursor - 1)
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        input.remove(idx);
                        idx
                    };
                    let _ = byte_idx;
                    *self.state.active_cursor_mut() -= 1;
                    self.invalidate_on_edit();
                    // Memory searches as you type — re-run on delete too (an
                    // empty input falls back to the filesystem browse).
                    if self.state.mode == ActiveMode::Memory {
                        self.state.memory.focused_pane = MemoryPane::List;
                        self.dispatch_memory();
                    }
                }
            }
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                let cursor = self.state.active_cursor();
                {
                    let input = self.state.active_input_mut();
                    let byte_idx = input
                        .char_indices()
                        .nth(cursor)
                        .map(|(i, _)| i)
                        .unwrap_or_else(|| input.len());
                    input.insert(byte_idx, c);
                }
                *self.state.active_cursor_mut() += 1;
                self.invalidate_on_edit();
                // Memory mode searches as you type: return focus to the list
                // and re-run the query so results track the input live. This
                // frees Enter to focus the detail pane instead.
                if self.state.mode == ActiveMode::Memory {
                    self.state.memory.focused_pane = MemoryPane::List;
                    self.dispatch_memory();
                }
            }
            _ => {}
        }
    }

    // ── Mode + pane switching ─────────────────────────────────────────────────

    /// Cycle the active mode forward (`delta > 0`) or backward.
    fn cycle_mode(&mut self, delta: i32) {
        let order = [
            ActiveMode::Search,
            ActiveMode::Impact,
            ActiveMode::Context,
            ActiveMode::Memory,
        ];
        let cur = order
            .iter()
            .position(|m| *m == self.state.mode)
            .unwrap_or(0);
        let next = (cur as i32 + delta).rem_euclid(order.len() as i32) as usize;
        self.state.mode = order[next].clone();
        // Entering Memory for the first time: browse everything so the list
        // isn't empty before the user types a query.
        if self.state.mode == ActiveMode::Memory && !self.state.memory.browsed {
            self.dispatch_memory();
        }
    }

    fn focus_left(&mut self) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.focused_pane = SearchPane::List;
            }
            ActiveMode::Impact => {
                self.state.impact.focused_pane = ImpactPane::EntryPoints;
            }
            ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::EntryPoints;
            }
            ActiveMode::Memory => {
                self.state.memory.focused_pane = MemoryPane::List;
            }
        }
    }

    fn focus_right(&mut self) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.focused_pane = SearchPane::Code;
            }
            ActiveMode::Impact => {
                self.state.impact.focused_pane = ImpactPane::Chain;
                // Reset chain selection whenever we enter the pane.
                self.state.impact.chain_selected = 0;
            }
            ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::Tree;
                self.state.context.chain_selected = 0;
            }
            ActiveMode::Memory => {
                self.state.memory.focused_pane = MemoryPane::Detail;
                self.state.memory.detail_scroll = 0;
            }
        }
    }

    /// After the query text changes, discard the stale results and return focus
    /// to the input/list pane, so the next Enter re-runs the analysis (rather
    /// than drilling into a now-outdated right pane). Memory is excluded — it
    /// searches live on every keystroke.
    fn invalidate_on_edit(&mut self) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.focused_pane = SearchPane::List;
            }
            ActiveMode::Impact => {
                self.state.impact.analysis = None;
                self.state.impact.focused_pane = ImpactPane::EntryPoints;
                self.state.impact.chain_snippet = None;
                self.state.impact.chain_snippet_loading = false;
                self.state.impact.chain_snippet_pending_key = None;
                self.state.impact.chain_snippet_scroll = 0;
            }
            ActiveMode::Context => {
                self.state.context.context = None;
                self.state.context.focused_pane = ContextPane::EntryPoints;
                self.state.context.chain_snippet = None;
                self.state.context.chain_snippet_loading = false;
                self.state.context.chain_snippet_pending_key = None;
                self.state.context.chain_snippet_scroll = 0;
            }
            ActiveMode::Memory => {}
        }
    }

    // ── Esc ───────────────────────────────────────────────────────────────────

    /// Esc backs out one level: an open code snippet closes first, otherwise the
    /// right/detail pane hands focus back to the left/list pane.
    fn handle_esc(&mut self) {
        match self.state.mode {
            ActiveMode::Impact => {
                if self.state.impact.chain_snippet.is_some() {
                    // Close the chain code view, back to chain navigation.
                    self.state.impact.chain_snippet = None;
                    self.state.impact.chain_snippet_loading = false;
                    self.state.impact.chain_snippet_pending_key = None;
                    self.state.impact.chain_snippet_scroll = 0;
                } else if self.state.impact.focused_pane == ImpactPane::Chain {
                    self.focus_left();
                }
            }
            ActiveMode::Context => {
                if self.state.context.chain_snippet.is_some() {
                    // Close the context code view, back to tree navigation.
                    self.state.context.chain_snippet = None;
                    self.state.context.chain_snippet_loading = false;
                    self.state.context.chain_snippet_pending_key = None;
                    self.state.context.chain_snippet_scroll = 0;
                } else if self.state.context.focused_pane == ContextPane::Tree {
                    self.focus_left();
                }
            }
            ActiveMode::Search => {
                if self.state.search.focused_pane == SearchPane::Code {
                    self.focus_left();
                }
            }
            ActiveMode::Memory => {
                if self.state.memory.focused_pane == MemoryPane::Detail {
                    self.focus_left();
                }
            }
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn navigate(&mut self, delta: i32) {
        match self.state.mode {
            ActiveMode::Search => {
                // Right pane focused → scroll the code panel.
                if self.state.search.focused_pane == SearchPane::Code {
                    self.state.search.snippet_scroll = bounded_scroll(
                        self.state.search.snippet_scroll,
                        delta * SCROLL_STEP as i32,
                    );
                    return;
                }
                let len = self.state.search.results.len();
                if len == 0 {
                    return;
                }
                self.state.search.selected = bounded_add(self.state.search.selected, delta, len);
                self.state.search.snippet_scroll = 0;
            }
            ActiveMode::Impact => {
                if self.state.impact.focused_pane == ImpactPane::Chain {
                    if self.state.impact.chain_snippet.is_some() {
                        // Scroll the chain code view.
                        self.state.impact.chain_snippet_scroll = bounded_scroll(
                            self.state.impact.chain_snippet_scroll,
                            delta * SCROLL_STEP as i32,
                        );
                    } else {
                        // Navigate within the call chain.
                        self.navigate_chain(delta);
                    }
                    return;
                }
                // Left pane (entry points).
                let len = self
                    .state
                    .impact
                    .analysis
                    .as_ref()
                    .map(|a| a.leaf_nodes().len())
                    .unwrap_or(0);
                if len == 0 {
                    return;
                }
                let old = self.state.impact.selected;
                self.state.impact.selected = bounded_add(old, delta, len);
                if self.state.impact.selected != old {
                    // Entry point changed — reset chain state.
                    self.state.impact.chain_selected = 0;
                    self.state.impact.chain_snippet = None;
                    self.state.impact.chain_snippet_loading = false;
                    self.state.impact.chain_snippet_pending_key = None;
                    self.state.impact.chain_snippet_scroll = 0;
                }
            }
            ActiveMode::Context => {
                if self.state.context.focused_pane == ContextPane::Tree {
                    if self.state.context.chain_snippet.is_some() {
                        self.state.context.chain_snippet_scroll = bounded_scroll(
                            self.state.context.chain_snippet_scroll,
                            delta * SCROLL_STEP as i32,
                        );
                    } else {
                        // Navigate through the full flat tree (callers + callees).
                        let flat = self
                            .state
                            .context
                            .context
                            .as_ref()
                            .map(|ctx| {
                                build_flat_tree_for_selected(ctx, self.state.context.selected)
                            })
                            .unwrap_or_default();
                        let len = flat.len();
                        if len > 0 {
                            self.state.context.chain_selected =
                                bounded_add(self.state.context.chain_selected, delta, len);
                            // Auto-scroll to keep the cursor visible.
                            let lines_idx = flat[self.state.context.chain_selected].lines_index;
                            let height = self.state.context.tree_pane_height.get() as usize;
                            let scroll = self.state.context.tree_scroll as usize;
                            if lines_idx < scroll {
                                self.state.context.tree_scroll = lines_idx as u16;
                            } else if height > 0 && lines_idx >= scroll + height {
                                self.state.context.tree_scroll =
                                    (lines_idx + 1).saturating_sub(height) as u16;
                            }
                        }
                    }
                    return;
                }
                // Left pane (entry points).
                let len = self
                    .state
                    .context
                    .context
                    .as_ref()
                    .map(|ctx| {
                        let leaf_count = leaf_caller_nodes(ctx).len();
                        // When there are no callers we show a single synthetic
                        // "callees only" entry so the right pane can render.
                        leaf_count.max(if ctx.total_callers == 0 { 1 } else { 0 })
                    })
                    .unwrap_or(0);
                if len == 0 {
                    return;
                }
                let old = self.state.context.selected;
                self.state.context.selected = bounded_add(old, delta, len);
                if self.state.context.selected != old {
                    self.state.context.chain_selected = 0;
                    self.state.context.chain_snippet = None;
                    self.state.context.chain_snippet_loading = false;
                    self.state.context.chain_snippet_pending_key = None;
                    self.state.context.chain_snippet_scroll = 0;
                }
            }
            ActiveMode::Memory => {
                // Detail pane focused → scroll the detail panel.
                if self.state.memory.focused_pane == MemoryPane::Detail {
                    self.state.memory.detail_scroll =
                        bounded_scroll(self.state.memory.detail_scroll, delta * SCROLL_STEP as i32);
                    return;
                }
                let len = self.state.memory.entries.len();
                if len == 0 {
                    return;
                }
                self.state.memory.selected = bounded_add(self.state.memory.selected, delta, len);
                self.state.memory.detail_scroll = 0;
            }
        }
    }

    fn navigate_chain(&mut self, delta: i32) {
        // Selectable rows = the caller path plus the root ◉ node at the end, so
        // `+ 1`. The root (the analysed symbol) is selectable too, mirroring the
        // Context tree.
        let len = self
            .state
            .impact
            .analysis
            .as_ref()
            .and_then(|a| {
                let leaves = a.leaf_nodes();
                let leaf = leaves.get(self.state.impact.selected).copied()?;
                Some(a.path_for_leaf(leaf).len() + 1)
            })
            .unwrap_or(0);
        if len == 0 {
            return;
        }
        self.state.impact.chain_selected =
            bounded_add(self.state.impact.chain_selected, delta, len);
    }

    fn scroll_code(&mut self, delta: i32) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.snippet_scroll =
                    bounded_scroll(self.state.search.snippet_scroll, delta);
            }
            ActiveMode::Impact => {
                if self.state.impact.chain_snippet.is_some() {
                    self.state.impact.chain_snippet_scroll =
                        bounded_scroll(self.state.impact.chain_snippet_scroll, delta);
                } else {
                    self.state.impact.flame_scroll =
                        bounded_scroll(self.state.impact.flame_scroll, delta);
                }
            }
            ActiveMode::Context => {
                if self.state.context.chain_snippet.is_some() {
                    self.state.context.chain_snippet_scroll =
                        bounded_scroll(self.state.context.chain_snippet_scroll, delta);
                } else {
                    self.state.context.tree_scroll =
                        bounded_scroll(self.state.context.tree_scroll, delta);
                }
            }
            ActiveMode::Memory => {
                self.state.memory.detail_scroll =
                    bounded_scroll(self.state.memory.detail_scroll, delta);
            }
        }
    }

    // ── Jump to impact / context ──────────────────────────────────────────────

    /// The call-graph symbol currently selected in the focused detail pane, if
    /// any — used as the target for the I / X jump keys. Resolves the selection
    /// per mode: a search result, or the selected node of an impact chain /
    /// context tree.
    fn selected_symbol(&self) -> Option<String> {
        match self.state.mode {
            ActiveMode::Search => self
                .state
                .search
                .results
                .get(self.state.search.selected)
                .and_then(|r| r.chunk().call_graph_name()),
            ActiveMode::Impact => {
                let analysis = self.state.impact.analysis.as_ref()?;
                let leaves = analysis.leaf_nodes();
                let leaf = leaves.get(self.state.impact.selected)?;
                let path = analysis.path_for_leaf(leaf);
                // The row after the caller path is the root ◉ (the analysed
                // symbol itself).
                if self.state.impact.chain_selected == path.len() {
                    Some(analysis.root_symbol.clone())
                } else {
                    path.get(self.state.impact.chain_selected)
                        .map(|n| n.symbol.clone())
                }
            }
            ActiveMode::Context => {
                let ctx = self.state.context.context.as_ref()?;
                let flat = build_flat_tree_for_selected(ctx, self.state.context.selected);
                flat.get(self.state.context.chain_selected)
                    .map(|n| n.symbol.clone())
            }
            ActiveMode::Memory => None,
        }
    }

    fn jump_to_impact(&mut self) {
        let Some(sym) = self.selected_symbol() else {
            return;
        };
        self.state.impact.cursor = sym.chars().count();
        self.state.impact.input = sym;
        self.state.impact.analysis = None;
        self.state.impact.selected = 0;
        self.state.impact.flame_scroll = 0;
        self.state.impact.chain_selected = 0;
        self.state.impact.chain_snippet = None;
        self.state.impact.chain_snippet_loading = false;
        self.state.impact.chain_snippet_pending_key = None;
        self.state.impact.chain_snippet_scroll = 0;
        self.state.impact.focused_pane = ImpactPane::EntryPoints;
        self.state.mode = ActiveMode::Impact;
        self.dispatch_impact();
    }

    // ── Jump search → context ─────────────────────────────────────────────────

    fn jump_to_context(&mut self) {
        let Some(sym) = self.selected_symbol() else {
            return;
        };
        // When jumping from a Search result, seed the context repository from
        // that result (unless a search-level filter is already active).
        if self.state.mode == ActiveMode::Search && self.state.search.repository.is_none() {
            self.state.context.repository = self
                .state
                .search
                .results
                .get(self.state.search.selected)
                .map(|r| r.chunk().repository_id().to_owned());
        }
        self.state.context.cursor = sym.chars().count();
        self.state.context.input = sym;
        self.state.context.context = None;
        self.state.context.selected = 0;
        self.state.context.tree_scroll = 0;
        self.state.context.chain_selected = 0;
        self.state.context.chain_snippet = None;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
        self.state.context.focused_pane = ContextPane::EntryPoints;
        self.state.mode = ActiveMode::Context;
        self.dispatch_context();
    }

    // ── Dispatch (cache-first) ────────────────────────────────────────────────

    fn dispatch_current(&mut self) {
        // Silently ignore Enter while models are still loading.
        if !self.state.models_ready {
            return;
        }
        match self.state.mode {
            // Search runs live as you type. From the results list, Enter drills
            // into the code pane (to read/scroll the selected result); from the
            // code pane it re-runs the search (a manual refresh).
            ActiveMode::Search => {
                if self.state.search.focused_pane == SearchPane::List
                    && !self.state.search.results.is_empty()
                {
                    self.focus_right();
                } else {
                    self.dispatch_search();
                }
            }
            // From the entry-points pane, Enter runs the analysis; once it has
            // results, Enter focuses the chain pane (to walk callers/callees).
            // In the chain pane, Enter loads the selected node's code.
            ActiveMode::Impact => match self.state.impact.focused_pane {
                ImpactPane::EntryPoints => {
                    if self.state.impact.analysis.is_some() {
                        self.focus_right();
                    } else {
                        self.dispatch_impact();
                    }
                }
                ImpactPane::Chain => {
                    if self.state.impact.chain_snippet.is_none()
                        && !self.state.impact.chain_snippet_loading
                    {
                        self.dispatch_chain_snippet();
                    }
                }
            },
            ActiveMode::Context => match self.state.context.focused_pane {
                ContextPane::EntryPoints => {
                    if self.state.context.context.is_some() {
                        self.focus_right();
                    } else {
                        self.dispatch_context();
                    }
                }
                ContextPane::Tree => {
                    if self.state.context.chain_snippet.is_none()
                        && !self.state.context.chain_snippet_loading
                    {
                        self.dispatch_context_snippet();
                    }
                }
            },
            // Memory searches live as you type, so Enter is free to drill in:
            // from the list it focuses the detail pane; from the detail pane it
            // is a no-op (Esc returns to the list).
            ActiveMode::Memory => {
                if self.state.memory.focused_pane == MemoryPane::List
                    && !self.state.memory.entries.is_empty()
                {
                    self.state.memory.focused_pane = MemoryPane::Detail;
                    self.state.memory.detail_scroll = 0;
                }
            }
        }
    }

    fn dispatch_search(&mut self) {
        let uc = match &self.search_uc {
            Some(uc) => Arc::clone(uc),
            None => return, // models not yet ready
        };

        let input = self.state.search.input.trim().to_string();
        if input.is_empty() {
            return;
        }

        let key = TuiCache::search_key(&input, self.state.search.repository.as_deref());

        if let Some(cached) = self.cache.searches.get(&key).cloned() {
            self.state.search.results = cached;
            self.state.search.selected = 0;
            self.state.search.snippet_scroll = 0;
            self.state.search.error = None;
            self.state.search.loading = false;
            self.state.search.pending_key = None;
            return;
        }

        if self.state.search.pending_key.as_deref() == Some(&key) {
            return;
        }

        if self.state.search.errored_key.as_deref() == Some(&key) {
            return;
        }

        self.state.search.loading = true;
        self.state.search.error = None;
        self.state.search.selected = 0;
        self.state.search.snippet_scroll = 0;
        self.state.search.pending_key = Some(key.clone());
        self.state.search.errored_key = None;

        let tx = self.event_tx.clone();
        let repository = self.state.search.repository.clone();

        tokio::spawn(async move {
            let mut q = SearchQuery::new(input)
                .with_limit(SEARCH_LIMIT)
                .with_text_search(true);
            if let Some(r) = repository {
                q = q.with_repositories(vec![r]);
            }
            let result = uc.execute(q).await.map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::SearchDone { key, result }) {
                debug!("SearchDone send failed (app already exited): {}", e);
            }
        });
    }

    fn dispatch_impact(&mut self) {
        let uc = match &self.impact_uc {
            Some(uc) => Arc::clone(uc),
            None => return, // models not yet ready
        };

        let symbol = self.state.impact.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }

        let key = TuiCache::impact_key(&symbol, self.state.impact.repository.as_deref());

        if let Some(cached) = self.cache.impacts.get(&key).cloned() {
            self.state.impact.analysis = Some(cached);
            self.state.impact.selected = 0;
            self.state.impact.flame_scroll = 0;
            self.state.impact.error = None;
            self.state.impact.loading = false;
            self.state.impact.pending_key = None;
            return;
        }

        if self.state.impact.pending_key.as_deref() == Some(&key) {
            return;
        }

        if self.state.impact.errored_key.as_deref() == Some(&key) {
            return;
        }

        self.state.impact.loading = true;
        self.state.impact.error = None;
        self.state.impact.analysis = None;
        self.state.impact.selected = 0;
        self.state.impact.flame_scroll = 0;
        self.state.impact.chain_selected = 0;
        self.state.impact.chain_snippet = None;
        self.state.impact.chain_snippet_loading = false;
        self.state.impact.chain_snippet_pending_key = None;
        self.state.impact.chain_snippet_scroll = 0;
        self.state.impact.pending_key = Some(key.clone());
        self.state.impact.errored_key = None;

        let tx = self.event_tx.clone();
        let repository = self.state.impact.repository.clone();

        if let Some(handle) = self.impact_task.take() {
            handle.abort();
        }
        self.impact_task = Some(tokio::spawn(async move {
            let result = uc
                .analyze(&symbol, repository.as_deref(), false)
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ImpactDone { key, result }) {
                debug!("ImpactDone send failed (app already exited): {}", e);
            }
        }));
    }

    fn dispatch_chain_snippet(&mut self) {
        let uc = match &self.snippet_uc {
            Some(uc) => Arc::clone(uc),
            None => return, // models not yet ready
        };

        // Extract the (repo_id, file_path, line) for the selected chain node.
        // The row after the caller path is the root ◉ (the analysed symbol); it
        // has no call-site location, so look it up by symbol name (line 0).
        let (node_coords, by_symbol) = {
            let analysis = match &self.state.impact.analysis {
                Some(a) => a,
                None => return,
            };
            let leaves = analysis.leaf_nodes();
            let leaf = match leaves.get(self.state.impact.selected) {
                Some(l) => *l,
                None => return,
            };
            let path = analysis.path_for_leaf(leaf);
            if self.state.impact.chain_selected == path.len() {
                // Root node: repo borrowed from the leaf, symbol as the "path".
                let repo = path
                    .first()
                    .map(|n| n.repository_id.clone())
                    .unwrap_or_default();
                ((repo, analysis.root_symbol.clone(), 0), true)
            } else {
                let node = match path.get(self.state.impact.chain_selected) {
                    Some(n) => *n,
                    None => return,
                };
                (
                    (
                        node.repository_id.clone(),
                        node.file_path.clone(),
                        node.line,
                    ),
                    false,
                )
            }
        };

        // For the root node, `node_coords.1` is the symbol name (line 0); for a
        // caller node it's a file path + line.
        let key = TuiCache::snippet_key(&node_coords.0, &node_coords.1, node_coords.2);

        if let Some(cached) = self.cache.snippets.get(&key).cloned() {
            self.state.impact.chain_snippet = cached;
            self.state.impact.chain_snippet_loading = false;
            self.state.impact.chain_snippet_pending_key = None;
            self.state.impact.chain_snippet_scroll = 0;
            return;
        }

        self.state.impact.chain_snippet_loading = true;
        self.state.impact.chain_snippet_pending_key = Some(key.clone());
        self.state.impact.chain_snippet_scroll = 0;

        let tx = self.event_tx.clone();
        let (repo_id, file_path, line) = node_coords;

        tokio::spawn(async move {
            let result = if by_symbol {
                uc.get_snippet_for_symbol(&repo_id, &file_path).await
            } else {
                uc.get_snippet(&repo_id, &file_path, line).await
            }
            .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ChainSnippetDone { key, result }) {
                debug!("ChainSnippetDone send failed (app already exited): {}", e);
            }
        });
    }

    fn dispatch_context(&mut self) {
        let uc = match &self.context_uc {
            Some(uc) => Arc::clone(uc),
            None => return, // models not yet ready
        };

        let symbol = self.state.context.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }

        let key = TuiCache::context_key(&symbol, self.state.context.repository.as_deref());

        if let Some(cached) = self.cache.contexts.get(&key).cloned() {
            self.state.context.context = Some(cached);
            self.state.context.selected = 0;
            self.state.context.tree_scroll = 0;
            self.state.context.error = None;
            self.state.context.loading = false;
            self.state.context.pending_key = None;
            return;
        }

        if self.state.context.pending_key.as_deref() == Some(&key) {
            return;
        }

        if self.state.context.errored_key.as_deref() == Some(&key) {
            return;
        }

        self.state.context.loading = true;
        self.state.context.error = None;
        self.state.context.context = None;
        self.state.context.selected = 0;
        self.state.context.tree_scroll = 0;
        self.state.context.chain_selected = 0;
        self.state.context.chain_snippet = None;
        self.state.context.chain_snippet_loading = false;
        self.state.context.chain_snippet_pending_key = None;
        self.state.context.chain_snippet_scroll = 0;
        self.state.context.pending_key = Some(key.clone());
        self.state.context.errored_key = None;

        let tx = self.event_tx.clone();
        let repository = self.state.context.repository.clone();

        if let Some(handle) = self.context_task.take() {
            handle.abort();
        }
        self.context_task = Some(tokio::spawn(async move {
            let result = uc
                .get_context(&symbol, repository.as_deref(), false)
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ContextDone { key, result }) {
                debug!("ContextDone send failed (app already exited): {}", e);
            }
        }));
    }

    fn dispatch_context_snippet(&mut self) {
        let uc = match &self.snippet_uc {
            Some(uc) => Arc::clone(uc),
            None => return,
        };

        let node_info = {
            let ctx = match &self.state.context.context {
                Some(c) => c,
                None => return,
            };

            // Use the flat tree to resolve the selected node (covers both callers
            // and callees by flat index).
            let flat = build_flat_tree_for_selected(ctx, self.state.context.selected);
            let node = match flat.get(self.state.context.chain_selected) {
                Some(n) => n,
                None => return,
            };

            (
                node.symbol.clone(),
                node.repository_id.clone(),
                node.file_path.clone(),
                node.line,
                node.is_callee,
            )
        };

        let (symbol, repo_id, file_path, line, is_callee) = node_info;

        // Callee nodes: look up the callee's own definition by symbol name.
        // Caller nodes: look up the call-site chunk (file_path + line).
        let key = if is_callee {
            TuiCache::snippet_key(&repo_id, &symbol, 0)
        } else {
            TuiCache::snippet_key(&repo_id, &file_path, line)
        };

        if let Some(cached) = self.cache.snippets.get(&key).cloned() {
            self.state.context.chain_snippet = cached;
            self.state.context.chain_snippet_loading = false;
            self.state.context.chain_snippet_pending_key = None;
            self.state.context.chain_snippet_scroll = 0;
            return;
        }

        self.state.context.chain_snippet_loading = true;
        self.state.context.chain_snippet_pending_key = Some(key.clone());
        self.state.context.chain_snippet_scroll = 0;

        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let result = if is_callee {
                uc.get_snippet_for_symbol(&repo_id, &symbol)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                uc.get_snippet(&repo_id, &file_path, line)
                    .await
                    .map_err(|e| e.to_string())
            };
            if let Err(e) = tx.send(TuiEvent::ContextSnippetDone { key, result }) {
                debug!("ContextSnippetDone send failed (app already exited): {}", e);
            }
        });
    }

    fn dispatch_memory(&mut self) {
        let uc = match &self.memory_uc {
            Some(uc) => Arc::clone(uc),
            None => return, // models not yet ready
        };

        // Empty input is valid here — it is the "browse everything" request.
        let input = self.state.memory.input.trim().to_string();
        // Mark that the initial browse has happened so entering Memory again
        // doesn't re-dispatch it.
        self.state.memory.browsed = true;

        let key = TuiCache::memory_key(&input);

        if let Some(cached) = self.cache.memories.get(&key).cloned() {
            self.state.memory.entries = cached;
            self.state.memory.selected = 0;
            self.state.memory.detail_scroll = 0;
            self.state.memory.error = None;
            self.state.memory.loading = false;
            self.state.memory.pending_key = None;
            return;
        }

        if self.state.memory.pending_key.as_deref() == Some(&key) {
            return;
        }
        if self.state.memory.errored_key.as_deref() == Some(&key) {
            return;
        }

        self.state.memory.loading = true;
        self.state.memory.error = None;
        self.state.memory.selected = 0;
        self.state.memory.detail_scroll = 0;
        self.state.memory.pending_key = Some(key.clone());
        self.state.memory.errored_key = None;

        let tx = self.event_tx.clone();

        if let Some(handle) = self.memory_task.take() {
            handle.abort();
        }
        self.memory_task = Some(tokio::spawn(async move {
            let result = uc
                .execute(&input, MEMORY_LIMIT)
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::MemoryDone { key, result }) {
                debug!("MemoryDone send failed (app already exited): {}", e);
            }
        }));
    }

    // ── Handle results ────────────────────────────────────────────────────────

    fn handle_app_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::ContainerReady(result) => {
                match result {
                    Ok(container) => {
                        self.search_uc = Some(Arc::new(container.search_use_case()));
                        self.impact_uc = Some(Arc::new(container.impact_use_case()));
                        self.snippet_uc = Some(Arc::new(container.snippet_lookup_use_case()));
                        self.context_uc = Some(Arc::new(container.context_use_case()));
                        // Memory browse is optional — if the store can't be
                        // opened, Memory mode simply stays empty rather than
                        // failing the whole TUI.
                        self.memory_uc = container
                            .memory_browse_use_case()
                            .map(Arc::new)
                            .map_err(|e| warn!("memory store unavailable in TUI: {e}"))
                            .ok();
                        self.state.models_ready = true;
                        // If the user had pre-typed a query (via --query CLI arg),
                        // auto-dispatch it now that models are ready.
                        match self.state.mode {
                            ActiveMode::Search if !self.state.search.input.is_empty() => {
                                self.dispatch_search();
                            }
                            ActiveMode::Impact if !self.state.impact.input.is_empty() => {
                                self.dispatch_impact();
                            }
                            ActiveMode::Context if !self.state.context.input.is_empty() => {
                                self.dispatch_context();
                            }
                            // Memory mode browses on entry (even with no query).
                            ActiveMode::Memory => self.dispatch_memory(),
                            _ => {}
                        }
                    }
                    Err(e) => {
                        // Show the error in the active pane and let the user quit.
                        warn!("container init failed: {}", e);
                        match self.state.mode {
                            ActiveMode::Search => {
                                self.state.search.error = Some(format!("Model load error: {e}"));
                            }
                            ActiveMode::Impact => {
                                self.state.impact.error = Some(format!("Model load error: {e}"));
                            }
                            ActiveMode::Context => {
                                self.state.context.error = Some(format!("Model load error: {e}"));
                            }
                            ActiveMode::Memory => {
                                self.state.memory.error = Some(format!("Model load error: {e}"));
                            }
                        }
                    }
                }
            }
            TuiEvent::SearchDone { key, result } => {
                if self.state.search.pending_key.as_deref() != Some(&key) {
                    return;
                }
                self.state.search.pending_key = None;
                self.state.search.loading = false;
                match result {
                    Ok(results) => {
                        self.cache.searches.insert(key, results.clone());
                        self.state.search.results = results;
                        self.state.search.selected = 0;
                        self.state.search.snippet_scroll = 0;
                    }
                    Err(e) => {
                        self.state.search.errored_key = Some(key);
                        self.state.search.error = Some(e);
                    }
                }
            }
            TuiEvent::ImpactDone { key, result } => {
                if self.state.impact.pending_key.as_deref() != Some(&key) {
                    return;
                }
                self.state.impact.pending_key = None;
                self.state.impact.loading = false;
                match result {
                    Ok(analysis) => {
                        self.cache.impacts.insert(key, analysis.clone());
                        self.state.impact.analysis = Some(analysis);
                        self.state.impact.selected = 0;
                        self.state.impact.flame_scroll = 0;
                    }
                    Err(e) => {
                        self.state.impact.errored_key = Some(key);
                        self.state.impact.error = Some(e);
                    }
                }
            }
            TuiEvent::ChainSnippetDone { key, result } => {
                if self.state.impact.chain_snippet_pending_key.as_ref() != Some(&key) {
                    return;
                }
                self.state.impact.chain_snippet_pending_key = None;
                self.state.impact.chain_snippet_loading = false;
                match result {
                    Ok(chunk) => {
                        self.cache.snippets.insert(key, chunk.clone());
                        self.state.impact.chain_snippet = chunk;
                        self.state.impact.chain_snippet_scroll = 0;
                    }
                    Err(e) => {
                        warn!("chain snippet lookup failed: {}", e);
                    }
                }
            }
            TuiEvent::ContextDone { key, result } => {
                if self.state.context.pending_key.as_deref() != Some(&key) {
                    return;
                }
                self.state.context.pending_key = None;
                self.state.context.loading = false;
                match result {
                    Ok(ctx) => {
                        self.cache.contexts.insert(key, ctx.clone());
                        self.state.context.context = Some(ctx);
                        self.state.context.selected = 0;
                        self.state.context.tree_scroll = 0;
                    }
                    Err(e) => {
                        self.state.context.errored_key = Some(key);
                        self.state.context.error = Some(e);
                    }
                }
            }
            TuiEvent::ContextSnippetDone { key, result } => {
                if self.state.context.chain_snippet_pending_key.as_ref() != Some(&key) {
                    return;
                }
                self.state.context.chain_snippet_pending_key = None;
                self.state.context.chain_snippet_loading = false;
                match result {
                    Ok(chunk) => {
                        self.cache.snippets.insert(key, chunk.clone());
                        self.state.context.chain_snippet = chunk;
                        self.state.context.chain_snippet_scroll = 0;
                    }
                    Err(e) => {
                        warn!("context snippet lookup failed: {}", e);
                    }
                }
            }
            TuiEvent::MemoryDone { key, result } => {
                if self.state.memory.pending_key.as_deref() != Some(&key) {
                    return;
                }
                self.state.memory.pending_key = None;
                self.state.memory.loading = false;
                match result {
                    Ok(entries) => {
                        self.cache.memories.insert(key, entries.clone());
                        self.state.memory.entries = entries;
                        self.state.memory.selected = 0;
                        self.state.memory.detail_scroll = 0;
                    }
                    Err(e) => {
                        self.state.memory.errored_key = Some(key);
                        self.state.memory.error = Some(e);
                    }
                }
            }
        }
    }
}

// ── Utility ───────────────────────────────────────────────────────────────────

fn bounded_add(current: usize, delta: i32, len: usize) -> usize {
    let next = current as i64 + delta as i64;
    next.clamp(0, len as i64 - 1) as usize
}

fn bounded_scroll(current: u16, delta: i32) -> u16 {
    (current as i32 + delta).clamp(0, u16::MAX as i32) as u16
}
