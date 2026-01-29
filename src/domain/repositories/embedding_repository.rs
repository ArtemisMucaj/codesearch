use async_trait::async_trait;

use crate::domain::{DomainError, Embedding, SearchQuery, SearchResult};

/// Repository trait for embedding vector persistence and similarity search.
#[async_trait]
pub trait EmbeddingRepository: Send + Sync {
    async fn save(&self, embedding: &Embedding) -> Result<(), DomainError>;
    async fn save_batch(&self, embeddings: &[Embedding]) -> Result<(), DomainError>;
    async fn find_by_chunk_id(&self, chunk_id: &str) -> Result<Option<Embedding>, DomainError>;
    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError>;
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;
    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError>;
    async fn count(&self) -> Result<u64, DomainError>;
}
