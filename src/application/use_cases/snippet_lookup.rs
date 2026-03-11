use std::sync::Arc;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError};

/// Retrieves an indexed [`CodeChunk`] for a reference location shown in the TUI.
///
/// Given a file path and a line number (as returned by [`ContextNode`] or
/// [`ImpactNode`]), this use case queries the vector store for the chunks that
/// belong to that file and returns the smallest chunk whose line range contains
/// the reference line. Code is therefore always sourced from the indexed store,
/// never from the live filesystem.
pub struct SnippetLookupUseCase {
    vector_repo: Arc<dyn VectorRepository>,
}

impl SnippetLookupUseCase {
    pub fn new(vector_repo: Arc<dyn VectorRepository>) -> Self {
        Self { vector_repo }
    }

    /// Return the content of the indexed chunk that contains `line` in `file_path`.
    ///
    /// `repository_id` may be an empty string to search across all repositories.
    /// Returns `None` when no matching chunk is found (e.g. file not indexed).
    pub async fn get_snippet(
        &self,
        repository_id: &str,
        file_path: &str,
        line: u32,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let chunks = self
            .vector_repo
            .find_chunks_by_file(repository_id, file_path)
            .await
            .map_err(|e| {
                DomainError::storage(format!(
                    "snippet lookup for '{file_path}' in repository '{repository_id}': {e}"
                ))
            })?;

        // Prefer the smallest chunk whose range fully contains the reference line
        // so we show the tightest relevant context (e.g. a function rather than a module).
        let best = chunks
            .iter()
            .filter(|c| c.start_line() <= line && c.end_line() >= line)
            .min_by_key(|c| c.end_line().saturating_sub(c.start_line()));

        Ok(best.cloned())
    }

    /// Return the definition chunk for a symbol given only its name.
    ///
    /// Used for callee nodes in the Context tree view where only the callee symbol
    /// name is known — the stored `file_path`/`line` on a callee `ContextNode`
    /// point to the call-site (inside the root symbol), not the callee's definition.
    pub async fn get_snippet_for_symbol(
        &self,
        repository_id: &str,
        symbol: &str,
    ) -> Result<Option<CodeChunk>, DomainError> {
        self.vector_repo
            .find_chunk_by_symbol(repository_id, symbol)
            .await
            .map_err(|e| {
                DomainError::storage(format!(
                    "symbol snippet lookup for '{symbol}' in repository '{repository_id}': {e}"
                ))
            })
    }
}
