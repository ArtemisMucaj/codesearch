use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

/// Generates vector embeddings from code and queries.
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError>;

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError>;

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError>;

    fn config(&self) -> &EmbeddingConfig;
}
