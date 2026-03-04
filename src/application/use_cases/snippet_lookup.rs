use std::sync::Arc;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError};

/// Retrieves an indexed [`CodeChunk`] for a reference location shown in the TUI.
///
/// Given a file path and a line number (as returned by [`ContextEdge`] or
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
            .await?;

        // Prefer the smallest chunk whose range fully contains the reference line
        // so we show the tightest relevant context (e.g. a function rather than a module).
        let best = chunks
            .iter()
            .filter(|c| c.start_line() <= line && c.end_line() >= line)
            .min_by_key(|c| c.end_line().saturating_sub(c.start_line()));

        Ok(best.cloned())
    }
}
