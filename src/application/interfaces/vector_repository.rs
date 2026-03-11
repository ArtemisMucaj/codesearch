use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

/// Vector storage and similarity search operations.
#[async_trait]
pub trait VectorRepository: Send + Sync {
    async fn save_batch(
        &self,
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<(), DomainError>;

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError>;

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;

    /// Delete all chunks for a specific file path within a repository.
    /// Returns the number of chunks deleted.
    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError>;

    /// Similarity search. When `query.is_text_search()` is true, implementations should
    /// additionally run keyword (BM25-style) matching and fuse both result lists via
    /// Reciprocal Rank Fusion before returning. Backends that cannot perform text
    /// search may silently fall back to semantic-only results.
    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError>;

    async fn count(&self) -> Result<u64, DomainError>;

    /// Return all chunks stored for a given file path within a repository.
    ///
    /// Used by the TUI snippet-lookup use case to retrieve indexed source code
    /// for a given reference location without performing a similarity search.
    /// The default no-op preserves backwards compatibility for adapters that do
    /// not need to support snippet lookup (e.g. mock / in-memory test adapters).
    async fn find_chunks_by_file(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeChunk>, DomainError> {
        let _ = (repository_id, file_path);
        Ok(vec![])
    }

    /// Return the chunk whose `symbol_name` best matches `symbol` within a repository.
    ///
    /// Used by the TUI to show the definition of a callee symbol when only the symbol
    /// name is known (not its definition file/line).  The default no-op preserves
    /// backwards compatibility for adapters that do not need this capability.
    async fn find_chunk_by_symbol(
        &self,
        repository_id: &str,
        symbol: &str,
    ) -> Result<Option<CodeChunk>, DomainError> {
        let _ = (repository_id, symbol);
        Ok(None)
    }

    /// Called once after a batch of writes to finalise any deferred work
    /// (e.g. rebuilding a full-text search index). The default implementation
    /// is a no-op; backends that maintain auxiliary indexes should override it.
    async fn flush(&self) -> Result<(), DomainError> {
        Ok(())
    }
}
