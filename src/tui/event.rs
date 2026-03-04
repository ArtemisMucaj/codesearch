use crate::application::{ImpactAnalysis, SymbolContext};
use crate::domain::{CodeChunk, SearchResult};
use crate::tui::cache::SnippetKey;

/// Events produced either by background async tasks or by the crossterm event loop.
pub enum TuiEvent {
    /// Search use case completed; `key` is the cache key to store under.
    SearchDone {
        key: String,
        result: Result<Vec<SearchResult>, String>,
    },
    /// Impact analysis use case completed.
    ImpactDone {
        key: String,
        result: Result<ImpactAnalysis, String>,
    },
    /// Symbol context use case completed.
    ContextDone {
        key: String,
        result: Result<SymbolContext, String>,
    },
    /// Snippet lookup for the context right-pane completed.
    SnippetDone {
        key: SnippetKey,
        result: Result<Option<CodeChunk>, String>,
    },
}
