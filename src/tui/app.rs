use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::application::{
    ImpactAnalysisUseCase, SearchCodeUseCase, SnippetLookupUseCase, SymbolContextUseCase,
};
use crate::domain::SearchQuery;

use super::cache::TuiCache;
use super::event::TuiEvent;
use super::state::{ActiveMode, AppState, ContextPane};
use super::views;

const SEARCH_LIMIT: usize = 20;
const IMPACT_DEPTH: usize = 5;
const SCROLL_STEP: u16 = 5;
const DEFAULT_CONTEXT_LIMIT: u32 = 50;

pub struct TuiApp {
    state: AppState,
    cache: TuiCache,
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
            cache: TuiCache::default(),
            search_uc,
            impact_uc,
            context_uc,
            snippet_uc,
            event_tx: tx,
            event_rx: rx,
        }
    }

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
            KeyCode::Tab => {
                self.state.mode = match self.state.mode {
                    ActiveMode::Search => ActiveMode::Impact,
                    ActiveMode::Impact => ActiveMode::Context,
                    ActiveMode::Context => ActiveMode::Search,
                };
            }
            KeyCode::Enter => self.dispatch_current(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_to_impact();
            }
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.navigate(-1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.navigate(1),
            KeyCode::Left if self.state.mode == ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::Callers;
                self.load_context_snippet();
            }
            KeyCode::Right if self.state.mode == ActiveMode::Context => {
                self.state.context.focused_pane = ContextPane::Callees;
                self.load_context_snippet();
            }
            KeyCode::PageUp => self.scroll_code(-(SCROLL_STEP as i32)),
            KeyCode::PageDown => self.scroll_code(SCROLL_STEP as i32),
            KeyCode::Backspace => {
                self.state.active_input_mut().pop();
            }
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE
                    || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.state.active_input_mut().push(c);
            }
            _ => {}
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn navigate(&mut self, delta: i32) {
        match self.state.mode {
            ActiveMode::Search => {
                let len = self.state.search.results.len();
                if len == 0 {
                    return;
                }
                self.state.search.selected =
                    bounded_add(self.state.search.selected, delta, len);
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
            self.state.impact.input = sym;
            self.state.impact.analysis = None;
            self.state.impact.selected = 0;
            self.state.impact.flame_scroll = 0;
            self.state.mode = ActiveMode::Impact;
            self.dispatch_impact();
        }
    }

    // ── Dispatch (cache-first) ────────────────────────────────────────────────

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

        // Already waiting for a result with this exact key — do not spawn a duplicate.
        if self.state.search.pending_key.as_deref() == Some(&key) {
            return;
        }

        self.state.search.loading = true;
        self.state.search.error = None;
        self.state.search.selected = 0;
        self.state.search.snippet_scroll = 0;
        self.state.search.pending_key = Some(key.clone());

        let uc = Arc::clone(&self.search_uc);
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
        let symbol = self.state.impact.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }

        let key = TuiCache::impact_key(
            &symbol,
            IMPACT_DEPTH,
            self.state.impact.repository.as_deref(),
        );

        if let Some(cached) = self.cache.impacts.get(&key).cloned() {
            self.state.impact.analysis = Some(cached);
            self.state.impact.selected = 0;
            self.state.impact.flame_scroll = 0;
            self.state.impact.error = None;
            self.state.impact.loading = false;
            self.state.impact.pending_key = None;
            return;
        }

        // Already waiting for a result with this exact key — do not spawn a duplicate.
        if self.state.impact.pending_key.as_deref() == Some(&key) {
            return;
        }

        self.state.impact.loading = true;
        self.state.impact.error = None;
        self.state.impact.selected = 0;
        self.state.impact.flame_scroll = 0;
        self.state.impact.pending_key = Some(key.clone());

        let uc = Arc::clone(&self.impact_uc);
        let tx = self.event_tx.clone();
        let repository = self.state.impact.repository.clone();

        tokio::spawn(async move {
            let result = uc
                .analyze(&symbol, IMPACT_DEPTH, repository.as_deref())
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ImpactDone { key, result }) {
                debug!("ImpactDone send failed (app already exited): {}", e);
            }
        });
    }

    fn dispatch_context(&mut self) {
        let symbol = self.state.context.input.trim().to_string();
        if symbol.is_empty() {
            return;
        }

        let key = TuiCache::context_key(&symbol, self.state.context.repository.as_deref());

        if let Some(cached) = self.cache.contexts.get(&key).cloned() {
            self.state.context.context = Some(cached);
            self.state.context.selected_caller = 0;
            self.state.context.selected_callee = 0;
            self.state.context.snippet_scroll = 0;
            self.state.context.error = None;
            self.state.context.loading = false;
            self.state.context.pending_key = None;
            // Clear stale snippet state before load_context_snippet so that if
            // the new context has no edges the old snippet is not left visible.
            self.state.context.snippet = None;
            self.state.context.snippet_loading = false;
            self.state.context.pending_snippet_key = None;
            self.load_context_snippet();
            return;
        }

        // Already waiting for a result with this exact key — do not spawn a duplicate.
        if self.state.context.pending_key.as_deref() == Some(&key) {
            return;
        }

        self.state.context.loading = true;
        self.state.context.error = None;
        self.state.context.snippet = None;
        self.state.context.snippet_loading = false;
        self.state.context.selected_caller = 0;
        self.state.context.selected_callee = 0;
        self.state.context.snippet_scroll = 0;
        self.state.context.pending_key = Some(key.clone());
        // Invalidate any in-flight snippet task so its SnippetDone is discarded.
        self.state.context.pending_snippet_key = None;

        let uc = Arc::clone(&self.context_uc);
        let tx = self.event_tx.clone();
        let repository = self.state.context.repository.clone();

        tokio::spawn(async move {
            let result = uc
                .get_context(&symbol, repository.as_deref(), Some(DEFAULT_CONTEXT_LIMIT))
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::ContextDone { key, result }) {
                debug!("ContextDone send failed (app already exited): {}", e);
            }
        });
    }

    /// Serve the snippet for the currently focused context edge, cache-first.
    /// Not-found results (Ok(None)) are cached to avoid redundant round-trips;
    /// transient errors are not cached so the lookup can be retried on next navigation.
    fn load_context_snippet(&mut self) {
        let s = &mut self.state.context;

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
            None => {
                s.snippet = None;
                s.snippet_loading = false;
                s.pending_snippet_key = None;
                return;
            }
        };

        let repository_id = s.repository.clone().unwrap_or_default();
        let cache_key = TuiCache::snippet_key(&repository_id, &edge.file_path, edge.line);

        // Cache hit (including not-found results stored as None).
        if let Some(cached) = self.cache.snippets.get(&cache_key).cloned() {
            s.snippet = cached;
            s.snippet_scroll = 0;
            s.snippet_loading = false;
            s.pending_snippet_key = None;
            return;
        }

        s.snippet = None;
        s.snippet_scroll = 0;
        s.snippet_loading = true;
        s.pending_snippet_key = Some(cache_key.clone());

        let uc = Arc::clone(&self.snippet_uc);
        let tx = self.event_tx.clone();

        tokio::spawn(async move {
            let result = uc
                .get_snippet(&repository_id, &edge.file_path, edge.line)
                .await
                .map_err(|e| e.to_string());
            if let Err(e) = tx.send(TuiEvent::SnippetDone {
                key: cache_key,
                result,
            }) {
                debug!("SnippetDone send failed (app already exited): {}", e);
            }
        });
    }

    // ── Handle results ────────────────────────────────────────────────────────

    fn handle_app_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::SearchDone { key, result } => {
                // Ignore results that are no longer for the active dispatch.
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
                        self.state.search.error = Some(e);
                    }
                }
            }
            TuiEvent::ImpactDone { key, result } => {
                // Ignore results that are no longer for the active dispatch.
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
                        self.state.impact.error = Some(e);
                    }
                }
            }
            TuiEvent::ContextDone { key, result } => {
                // Ignore results that are no longer for the active dispatch.
                if self.state.context.pending_key.as_deref() != Some(&key) {
                    return;
                }
                self.state.context.pending_key = None;
                self.state.context.loading = false;
                match result {
                    Ok(context) => {
                        self.cache.contexts.insert(key, context.clone());
                        self.state.context.context = Some(context);
                        self.state.context.selected_caller = 0;
                        self.state.context.selected_callee = 0;
                        self.state.context.snippet_scroll = 0;
                        self.load_context_snippet();
                    }
                    Err(e) => {
                        self.state.context.error = Some(e);
                    }
                }
            }
            TuiEvent::SnippetDone { key, result } => {
                // Ignore results from superseded snippet requests (e.g. rapid navigation).
                if self.state.context.pending_snippet_key.as_ref() != Some(&key) {
                    return;
                }
                self.state.context.pending_snippet_key = None;
                self.state.context.snippet_loading = false;
                match result {
                    Ok(chunk) => {
                        // Cache both Some (found) and None (explicitly not found).
                        self.cache.snippets.insert(key, chunk.clone());
                        self.state.context.snippet = chunk;
                        self.state.context.snippet_scroll = 0;
                    }
                    Err(e) => {
                        // Transient backend error: log but do not cache so the
                        // lookup can be retried on the next navigation.
                        warn!("snippet lookup failed: {}", e);
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
