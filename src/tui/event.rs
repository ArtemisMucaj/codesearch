use crate::application::ImpactAnalysis;
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
    /// Snippet lookup for a selected impact chain node completed.
    ChainSnippetDone {
        key: SnippetKey,
        result: Result<Option<CodeChunk>, String>,
    },
}
