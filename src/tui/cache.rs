use std::collections::HashMap;

use crate::application::ImpactAnalysis;
use crate::domain::{CodeChunk, SearchResult};

/// Lookup key for a snippet: `(repository_id, file_path, line)`.
pub type SnippetKey = (String, String, u32);

/// Session-scoped result cache for the TUI.
///
/// All three query types (search, impact, snippet) are memoised here for the
/// lifetime of a `codesearch tui` session.  A cache hit means no async task is
/// spawned and results appear instantly, which matters because the underlying
/// use cases (ONNX embedding, DuckDB BFS) can take hundreds of milliseconds on
/// first run.
#[derive(Default)]
pub struct TuiCache {
    pub searches: HashMap<String, Vec<SearchResult>>,
    pub impacts: HashMap<String, ImpactAnalysis>,
    pub snippets: HashMap<SnippetKey, Option<CodeChunk>>,
}

impl TuiCache {
    /// Build the cache key for a search query.
    pub fn search_key(query: &str, repository: Option<&str>) -> String {
        serde_json::to_string(&[query, repository.unwrap_or("")])
            .expect("serde_json serialisation of &str slice is infallible")
    }

    /// Build the cache key for an impact analysis.
    pub fn impact_key(symbol: &str, depth: usize, repository: Option<&str>) -> String {
        serde_json::to_string(&(symbol, depth, repository.unwrap_or("")))
            .expect("serde_json serialisation of (&str, usize, &str) is infallible")
    }

    /// Build the cache key for a snippet lookup.
    pub fn snippet_key(repository_id: &str, file_path: &str, line: u32) -> SnippetKey {
        (repository_id.to_string(), file_path.to_string(), line)
    }
}
