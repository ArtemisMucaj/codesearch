use crate::application::{ImpactAnalysis, SymbolContext};
use crate::domain::{CodeChunk, SearchResult};

/// Events produced either by background async tasks or by the crossterm event loop.
pub enum TuiEvent {
    /// Search use case completed.
    SearchDone(Result<Vec<SearchResult>, String>),
    /// Impact analysis use case completed.
    ImpactDone(Result<ImpactAnalysis, String>),
    /// Symbol context use case completed.
    ContextDone(Result<SymbolContext, String>),
    /// Snippet lookup for the context right-pane completed.
    SnippetDone(Result<Option<CodeChunk>, String>),
}
