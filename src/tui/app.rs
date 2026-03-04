use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::application::{
    ImpactAnalysisUseCase, SearchCodeUseCase, SnippetLookupUseCase, SymbolContextUseCase,
};
use crate::domain::SearchQuery;

use super::event::TuiEvent;
use super::state::{ActiveMode, AppState, ContextPane};
use super::views;

const SEARCH_LIMIT: usize = 20;
const SCROLL_STEP: u16 = 5;

pub struct TuiApp {
    state: AppState,
    search_uc: Arc<SearchCodeUseCase>,
    impact_uc: Arc<ImpactAnalysisUseCase>,
    context_uc: Arc<SymbolContextUseCase>,
    snippet_uc: Arc<SnippetLookupUseCase>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
}

impl TuiApp {
    pub fn new(
        search_uc: Arc<SearchCodeUseCase>,
        impact_uc: Arc<ImpactAnalysisUseCase>,
        context_uc: Arc<SymbolContextUseCase>,
        snippet_uc: Arc<SnippetLookupUseCase>,
        repository: Option<String>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            state: AppState::new(repository),
            search_uc,
            impact_uc,
            context_uc,
            snippet_uc,
            event_tx: tx,
            event_rx: rx,
        }
    }

    /// Take over the terminal, run the interactive loop, and restore the
    /// terminal on exit (including on error).
    pub async fn run(mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        let result = self.run_loop(&mut terminal).await;
        ratatui::restore();
        result
    }

    async fn run_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut stream = EventStream::new();

        loop {
            terminal.draw(|f| views::render(f, &self.state))?;

            tokio::select! {
                // Results arriving from background tasks
                Some(app_ev) = self.event_rx.recv() => {
                    self.handle_app_event(app_ev);
                }
                // Crossterm keyboard / resize events
                maybe_ev = stream.next() => {
                    match maybe_ev {
                        Some(Ok(Event::Key(key))) => self.handle_key(key),
                        Some(Ok(Event::Resize(..))) => {}  // re-draw on next loop
                        _ => {}
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
            // ── Quit ──────────────────────────────────────────────────────────
            KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => {
                self.state.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.should_quit = true;
            }

            // ── Tab: cycle through modes ──────────────────────────────────────
            KeyCode::Tab => {
                self.state.mode = match self.state.mode {
                    ActiveMode::Search => ActiveMode::Impact,
                    ActiveMode::Impact => ActiveMode::Context,
                    ActiveMode::Context => ActiveMode::Search,
                };
            }

            // ── Execute current mode ──────────────────────────────────────────
            KeyCode::Enter => self.dispatch_current(),

            // ── Ctrl+Up in search: jump to impact for the selected symbol ─────
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_to_impact();
            }

            // ── Up/Down: navigate the result list in the current mode ─────────
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.navigate(-1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.navigate(1),

            // ── Left/Right: switch callers/callees focus in context mode ──────
            KeyCode::Left if self.state.mode == ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::Callers;
                self.load_context_snippet();
            }
            KeyCode::Right if self.state.mode == ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::Callees;
                self.load_context_snippet();
            }

            // ── Page scroll for the right pane ────────────────────────────────
            KeyCode::PageUp => self.scroll_code(-(SCROLL_STEP as i32)),
            KeyCode::PageDown => self.scroll_code(SCROLL_STEP as i32),

            // ── Text input ────────────────────────────────────────────────────
            KeyCode::Backspace => {
                self.state.active_input_mut().pop();
            }
            KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE
                || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.state.active_input_mut().push(c);
            }

            _ => {}
        }
    }

    // ── Navigation helpers ────────────────────────────────────────────────────

    fn navigate(&mut self, delta: i32) {
        match self.state.mode {
            ActiveMode::Search => {
                let len = self.state.search.results.len();
                if len == 0 {
                    return;
                }
                self.state.search.selected =
                    bounded_add(self.state.search.selected, delta, len);
                // Reset snippet scroll when selection changes.
                self.state.search.snippet_scroll = 0;
            }
            ActiveMode::Impact => {
                let len = self
                    .state
                    .impact
                    .analysis
                    .as_ref()
                    .map(|a| a.by_depth.iter().map(|d| d.len()).sum::<usize>())
                    .unwrap_or(0);
                if len == 0 {
                    return;
                }
                self.state.impact.selected = bounded_add(self.state.impact.selected, delta, len);
            }
            ActiveMode::Context => {
                let s = &mut self.state.context;
                match s.focused_pane {
                    ContextPane::Callers => {
                        let len = s.context.as_ref().map(|c| c.callers.len()).unwrap_or(0);
                        if len == 0 {
                            return;
                        }
                        s.selected_caller = bounded_add(s.selected_caller, delta, len);
                    }
                    ContextPane::Callees => {
                        let len = s.context.as_ref().map(|c| c.callees.len()).unwrap_or(0);
                        if len == 0 {
                            return;
                        }
                        s.selected_callee = bounded_add(s.selected_callee, delta, len);
                    }
                }
                // Load the snippet for the newly selected edge.
                self.load_context_snippet();
            }
        }
    }

    fn scroll_code(&mut self, delta: i32) {
        match self.state.mode {
            ActiveMode::Search => {
                self.state.search.snippet_scroll =
                    bounded_scroll(self.state.search.snippet_scroll, delta);
            }
            ActiveMode::Impact => {
                self.state.impact.flame_scroll =
                    bounded_scroll(self.state.impact.flame_scroll, delta);
            }
            ActiveMode::Context => {
                self.state.context.snippet_scroll =
                    bounded_scroll(self.state.context.snippet_scroll, delta);
            }
        }
    }

    // ── Jump to impact from search ────────────────────────────────────────────

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
            self.state.impact.input = sym;
            self.state.impact.analysis = None;
            self.state.impact.selected = 0;
            self.state.impact.flame_scroll = 0;
            self.state.mode = ActiveMode::Impact;
            self.dispatch_impact();
        }
    }

    // ── Dispatch async use cases ──────────────────────────────────────────────

    fn dispatch_current(&mut self) {
        match self.state.mode {
            ActiveMode::Search => self.dispatch_search(),
            ActiveMode::Impact => self.dispatch_impact(),
            ActiveMode::Context => self.dispatch_context(),
        }
    }

    fn dispatch_search(&mut self) {
        let input = self.state.search.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.state.search.loading = true;
        self.state.search.error = None;
        self.state.search.selected = 0;
        self.state.search.snippet_scroll = 0;

        let uc = Arc::clone(&self.search_uc);
        let tx = self.event_tx.clone();
        let repository = self.state.search.repository.clone();

        tokio::spawn(async move {
            let mut q = SearchQuery::new(input).with_limit(SEARCH_LIMIT).with_text_search(true);
            if let Some(r) = repository {
                q = q.with_repositories(vec![r]);
            }
            let result = uc.execute(q).await.map_err(|e| e.to_string());
            tx.send(TuiEvent::SearchDone(result)).ok();
        });
    }

    fn dispatch_impact(&mut self) {
        let symbol = self.state.impact.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }
        self.state.impact.loading = true;
        self.state.impact.error = None;
        self.state.impact.selected = 0;
        self.state.impact.flame_scroll = 0;

        let uc = Arc::clone(&self.impact_uc);
        let tx = self.event_tx.clone();
        let repository = self.state.impact.repository.clone();

        tokio::spawn(async move {
            let result = uc
                .analyze(&symbol, 5, repository.as_deref())
                .await
                .map_err(|e| e.to_string());
            tx.send(TuiEvent::ImpactDone(result)).ok();
        });
    }

    fn dispatch_context(&mut self) {
        let symbol = self.state.context.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }
        self.state.context.loading = true;
        self.state.context.error = None;
        self.state.context.snippet = None;
        self.state.context.selected_caller = 0;
        self.state.context.selected_callee = 0;
        self.state.context.snippet_scroll = 0;

        let uc = Arc::clone(&self.context_uc);
        let tx = self.event_tx.clone();
        let repository = self.state.context.repository.clone();

        tokio::spawn(async move {
            let result = uc
                .get_context(&symbol, repository.as_deref(), Some(50))
                .await
                .map_err(|e| e.to_string());
            tx.send(TuiEvent::ContextDone(result)).ok();
        });
    }

    /// Kick off a snippet lookup for the currently focused edge in context mode.
    fn load_context_snippet(&mut self) {
        let s = &mut self.state.context;
        s.snippet = None;
        s.snippet_scroll = 0;

        let edge = match s.focused_pane {
            ContextPane::Callers => s
                .context
                .as_ref()
                .and_then(|c| c.callers.get(s.selected_caller))
                .cloned(),
            ContextPane::Callees => s
                .context
                .as_ref()
                .and_then(|c| c.callees.get(s.selected_callee))
                .cloned(),
        };

        let edge = match edge {
            Some(e) => e,
            None => return,
        };

        s.snippet_loading = true;

        let uc = Arc::clone(&self.snippet_uc);
        let tx = self.event_tx.clone();
        let repository_id = s.repository.clone().unwrap_or_default();

        tokio::spawn(async move {
            let result = uc
                .get_snippet(&repository_id, &edge.file_path, edge.line)
                .await
                .map_err(|e| e.to_string());
            tx.send(TuiEvent::SnippetDone(result)).ok();
        });
    }

    // ── Handle results from background tasks ──────────────────────────────────

    fn handle_app_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::SearchDone(result) => {
                self.state.search.loading = false;
                match result {
                    Ok(results) => {
                        self.state.search.results = results;
                        self.state.search.selected = 0;
                        self.state.search.snippet_scroll = 0;
                    }
                    Err(e) => {
                        self.state.search.error = Some(e);
                    }
                }
            }
            TuiEvent::ImpactDone(result) => {
                self.state.impact.loading = false;
                match result {
                    Ok(analysis) => {
                        self.state.impact.analysis = Some(analysis);
                        self.state.impact.selected = 0;
                        self.state.impact.flame_scroll = 0;
                    }
                    Err(e) => {
                        self.state.impact.error = Some(e);
                    }
                }
            }
            TuiEvent::ContextDone(result) => {
                self.state.context.loading = false;
                match result {
                    Ok(context) => {
                        self.state.context.context = Some(context);
                        self.state.context.selected_caller = 0;
                        self.state.context.selected_callee = 0;
                        self.state.context.snippet_scroll = 0;
                        // Auto-load snippet for the first caller.
                        self.load_context_snippet();
                    }
                    Err(e) => {
                        self.state.context.error = Some(e);
                    }
                }
            }
            TuiEvent::SnippetDone(result) => {
                self.state.context.snippet_loading = false;
                match result {
                    Ok(chunk) => {
                        self.state.context.snippet = chunk;
                        self.state.context.snippet_scroll = 0;
                    }
                    Err(_) => {
                        self.state.context.snippet = None;
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
    (current as i32 + delta).max(0) as u16
}
