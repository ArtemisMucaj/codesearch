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

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError>;

    async fn count(&self) -> Result<u64, DomainError>;
}
