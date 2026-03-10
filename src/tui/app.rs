use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::application::{ImpactAnalysisUseCase, SearchCodeUseCase, SnippetLookupUseCase};
use crate::domain::SearchQuery;

use super::cache::TuiCache;
use super::event::TuiEvent;
use super::state::{ActiveMode, AppState, ImpactPane, SearchPane};
use super::views;
use crate::cli::TuiMode;

const SEARCH_LIMIT: usize = 20;
const SCROLL_STEP: u16 = 5;

pub struct TuiApp {
    state: AppState,
    cache: TuiCache,
    /// `None` while the background container task is still running.
    search_uc: Option<Arc<SearchCodeUseCase>>,
    /// `None` while the background container task is still running.
    impact_uc: Option<Arc<ImpactAnalysisUseCase>>,
    /// `None` while the background container task is still running.
    snippet_uc: Option<Arc<SnippetLookupUseCase>>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    impact_task: Option<tokio::task::JoinHandle<()>>,
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
        };
        Self {
            state: AppState::new(repository, initial_mode, query, false),
            cache: TuiCache::default(),
            search_uc: None,
            impact_uc: None,
            snippet_uc: None,
            event_tx,
            event_rx,
            impact_task: None,
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
        };
        Self {
            state: AppState::new(repository, initial_mode, query, true),
            cache: TuiCache::default(),
            search_uc: Some(search_uc),
            impact_uc: Some(impact_uc),
            snippet_uc: Some(snippet_uc),
            event_tx: tx,
            event_rx: rx,
            impact_task: None,
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
            KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => {
                self.state.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.should_quit = true;
            }
            KeyCode::Esc => self.handle_esc(),
            KeyCode::Tab => {
                self.state.mode = match self.state.mode {
                    ActiveMode::Search => ActiveMode::Impact,
                    ActiveMode::Impact => ActiveMode::Search,
                };
            }
            KeyCode::Enter => self.dispatch_current(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_to_impact();
            }
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.navigate(-1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.navigate(1),
            // Ctrl+←/→ switch panes; plain ←/→ move the text cursor.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.focus_left();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.focus_right();
            }
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
                // Typing new text in Impact mode while the Chain pane is
                // focused would cause Enter to dispatch a chain-snippet load
                // rather than a new impact analysis.  Reset to the entry-point
                // pane so the next Enter searches correctly.
                if self.state.mode == ActiveMode::Impact
                    && self.state.impact.focused_pane == ImpactPane::Chain
                {
                    self.state.impact.focused_pane = ImpactPane::EntryPoints;
                    self.state.impact.chain_snippet = None;
                    self.state.impact.chain_snippet_loading = false;
                    self.state.impact.chain_snippet_pending_key = None;
                    self.state.impact.chain_snippet_scroll = 0;
                }
            }
            _ => {}
        }
    }

    // ── Pane focus switching ──────────────────────────────────────────────────

    fn focus_left(&mut self) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.focused_pane = SearchPane::List;
            }
            ActiveMode::Impact => {
                self.state.impact.focused_pane = ImpactPane::EntryPoints;
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
        }
    }

    // ── Esc ───────────────────────────────────────────────────────────────────

    fn handle_esc(&mut self) {
        if self.state.mode == ActiveMode::Impact && self.state.impact.chain_snippet.is_some() {
            // Return from chain code view to chain navigation.
            self.state.impact.chain_snippet = None;
            self.state.impact.chain_snippet_loading = false;
            self.state.impact.chain_snippet_pending_key = None;
            self.state.impact.chain_snippet_scroll = 0;
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
        }
    }

    fn navigate_chain(&mut self, delta: i32) {
        let len = self
            .state
            .impact
            .analysis
            .as_ref()
            .and_then(|a| {
                let leaves = a.leaf_nodes();
                let leaf = leaves.get(self.state.impact.selected).copied()?;
                Some(a.path_for_leaf(leaf).len())
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
        }
    }

    // ── Jump search → impact ──────────────────────────────────────────────────

    fn jump_to_impact(&mut self) {
        if self.state.mode != ActiveMode::Search {
            return;
        }
        let symbol = self
            .state
            .search
            .results
            .get(self.state.search.selected)
            .and_then(|r| {
                r.chunk()
                    .qualified_name()
                    .or_else(|| r.chunk().symbol_name().map(str::to_string))
            });

        if let Some(sym) = symbol {
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
    }

    // ── Dispatch (cache-first) ────────────────────────────────────────────────

    fn dispatch_current(&mut self) {
        // Silently ignore Enter while models are still loading.
        if !self.state.models_ready {
            return;
        }
        match self.state.mode {
            ActiveMode::Search => self.dispatch_search(),
            ActiveMode::Impact => {
                match self.state.impact.focused_pane {
                    ImpactPane::EntryPoints => self.dispatch_impact(),
                    ImpactPane::Chain => {
                        // Enter in the chain pane loads the selected node's code.
                        if self.state.impact.chain_snippet.is_none()
                            && !self.state.impact.chain_snippet_loading
                        {
                            self.dispatch_chain_snippet();
                        }
                    }
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
        let node_coords = {
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
            let node = match path.get(self.state.impact.chain_selected) {
                Some(n) => *n,
                None => return,
            };
            (
                node.repository_id.clone(),
                node.file_path.clone(),
                node.line,
            )
        };

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
            let result = uc
                .get_snippet(&repo_id, &file_path, line)
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ChainSnippetDone { key, result }) {
                debug!("ChainSnippetDone send failed (app already exited): {}", e);
            }
        });
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
