use std::sync::Arc;

use crate::application::ImpactAnalysis;
use crate::connector::api::container::Container;
use crate::domain::{CodeChunk, SearchResult};
use crate::tui::cache::SnippetKey;

/// Events produced either by background async tasks or by the crossterm event loop.
pub enum TuiEvent {
    /// Background container/model initialisation completed.
    /// Carries the ready `Container` (or an error string if init failed).
    ContainerReady(Result<Arc<Container>, String>),
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
