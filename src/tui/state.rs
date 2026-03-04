use crate::application::{ImpactAnalysis, SymbolContext};
use crate::domain::{CodeChunk, SearchResult};

/// Which input mode / view is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveMode {
    Search,
    Impact,
    Context,
}

/// Which pane in the context view has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextPane {
    Callers,
    Callees,
}

// ── Per-mode state ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct SearchState {
    pub input: String,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the code panel.
    pub snippet_scroll: u16,
    /// Optional repository filter forwarded to the use case.
    pub repository: Option<String>,
    /// Cache key of the most recently dispatched search request.
    pub pending_key: Option<String>,
}

#[derive(Debug, Default)]
pub struct ImpactState {
    pub input: String,
    pub analysis: Option<ImpactAnalysis>,
    /// Index into the flat affected-node list shown in the left pane.
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the flame graph.
    pub flame_scroll: u16,
    pub repository: Option<String>,
    /// Cache key of the most recently dispatched impact request.
    pub pending_key: Option<String>,
}

#[derive(Debug)]
pub struct ContextState {
    pub input: String,
    pub context: Option<SymbolContext>,
    pub selected_caller: usize,
    pub selected_callee: usize,
    pub focused_pane: ContextPane,
    /// Indexed chunk fetched from the store for the selected edge.
    pub snippet: Option<CodeChunk>,
    pub loading: bool,
    pub snippet_loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the code panel.
    pub snippet_scroll: u16,
    pub repository: Option<String>,
    /// Cache key of the most recently dispatched context request.
    pub pending_key: Option<String>,
}

impl Default for ContextState {
    fn default() -> Self {
        Self {
            input: String::new(),
            context: None,
            selected_caller: 0,
            selected_callee: 0,
            focused_pane: ContextPane::Callers,
            snippet: None,
            loading: false,
            snippet_loading: false,
            error: None,
            snippet_scroll: 0,
            repository: None,
            pending_key: None,
        }
    }
}

// ── Top-level app state ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AppState {
    pub mode: ActiveMode,
    pub search: SearchState,
    pub impact: ImpactState,
    pub context: ContextState,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(repository: Option<String>) -> Self {
        Self {
            mode: ActiveMode::Search,
            search: SearchState {
                repository: repository.clone(),
                ..Default::default()
            },
            impact: ImpactState {
                repository: repository.clone(),
                ..Default::default()
            },
            context: ContextState {
                repository,
                ..Default::default()
            },
            should_quit: false,
        }
    }

    /// The text currently typed in the active mode's input box.
    pub fn active_input(&self) -> &str {
        match self.mode {
            ActiveMode::Search => &self.search.input,
            ActiveMode::Impact => &self.impact.input,
            ActiveMode::Context => &self.context.input,
        }
    }

    pub fn active_input_mut(&mut self) -> &mut String {
        match self.mode {
            ActiveMode::Search => &mut self.search.input,
            ActiveMode::Impact => &mut self.impact.input,
            ActiveMode::Context => &mut self.context.input,
        }
    }

    pub fn is_loading(&self) -> bool {
        match self.mode {
            ActiveMode::Search => self.search.loading,
            ActiveMode::Impact => self.impact.loading,
            ActiveMode::Context => self.context.loading || self.context.snippet_loading,
        }
    }
}
