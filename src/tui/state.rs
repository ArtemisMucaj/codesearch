use crate::application::ImpactAnalysis;
use crate::domain::{CodeChunk, SearchResult};
use crate::tui::cache::SnippetKey;

/// Which input mode / view is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveMode {
    Search,
    Impact,
}

/// Which pane in the search view has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchPane {
    #[default]
    List,
    Code,
}

/// Which pane in the impact view has keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ImpactPane {
    #[default]
    EntryPoints,
    Chain,
}

// ── Per-mode state ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct SearchState {
    pub input: String,
    /// Cursor position within `input`, measured in characters (not bytes).
    pub cursor: usize,
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
    /// Cache key of the last request that returned an error. Prevents the
    /// event loop from re-dispatching the same failing query on key-repeat.
    pub errored_key: Option<String>,
    /// Which pane currently has keyboard focus.
    pub focused_pane: SearchPane,
}

#[derive(Debug, Default)]
pub struct ImpactState {
    pub input: String,
    /// Cursor position within `input`, measured in characters (not bytes).
    pub cursor: usize,
    pub analysis: Option<ImpactAnalysis>,
    /// Index into the leaf-node list shown in the left pane.
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Vertical scroll offset for the chain / code panel (right pane).
    pub flame_scroll: u16,
    pub repository: Option<String>,
    /// Cache key of the most recently dispatched impact request.
    pub pending_key: Option<String>,
    /// Cache key of the last request that returned an error.
    pub errored_key: Option<String>,
    /// Which pane currently has keyboard focus.
    pub focused_pane: ImpactPane,
    /// Selected node index within the current call chain (right pane).
    pub chain_selected: usize,
    /// Code snippet for the selected chain node (Some = code view is active).
    pub chain_snippet: Option<CodeChunk>,
    pub chain_snippet_loading: bool,
    /// Vertical scroll offset for the chain code view.
    pub chain_snippet_scroll: u16,
    /// Pending key for an in-flight chain snippet request.
    pub chain_snippet_pending_key: Option<SnippetKey>,
}

// ── Top-level app state ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AppState {
    pub mode: ActiveMode,
    pub search: SearchState,
    pub impact: ImpactState,
    pub should_quit: bool,
    /// `false` while the ONNX models are still loading in the background.
    /// The status bar displays a hint and `Enter` is held until this is `true`.
    pub models_ready: bool,
}

impl AppState {
    pub fn new(
        repository: Option<String>,
        initial_mode: ActiveMode,
        initial_query: Option<String>,
        models_ready: bool,
    ) -> Self {
        let mut state = Self {
            mode: initial_mode.clone(),
            search: SearchState {
                repository: repository.clone(),
                ..Default::default()
            },
            impact: ImpactState {
                repository,
                ..Default::default()
            },
            should_quit: false,
            models_ready,
        };
        if let Some(query) = initial_query {
            match initial_mode {
                ActiveMode::Search => {
                    state.search.cursor = query.chars().count();
                    state.search.input = query;
                }
                ActiveMode::Impact => {
                    state.impact.cursor = query.chars().count();
                    state.impact.input = query;
                }
            }
        }
        state
    }

    /// The text currently typed in the active mode's input box.
    pub fn active_input(&self) -> &str {
        match self.mode {
            ActiveMode::Search => &self.search.input,
            ActiveMode::Impact => &self.impact.input,
        }
    }

    pub fn active_input_mut(&mut self) -> &mut String {
        match self.mode {
            ActiveMode::Search => &mut self.search.input,
            ActiveMode::Impact => &mut self.impact.input,
        }
    }

    pub fn active_cursor(&self) -> usize {
        match self.mode {
            ActiveMode::Search => self.search.cursor,
            ActiveMode::Impact => self.impact.cursor,
        }
    }

    pub fn active_cursor_mut(&mut self) -> &mut usize {
        match self.mode {
            ActiveMode::Search => &mut self.search.cursor,
            ActiveMode::Impact => &mut self.impact.cursor,
        }
    }

    pub fn is_loading(&self) -> bool {
        match self.mode {
            ActiveMode::Search => self.search.loading,
            ActiveMode::Impact => self.impact.loading,
        }
    }
}
