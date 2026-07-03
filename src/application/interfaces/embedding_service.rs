use async_trait::async_trait;

use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

/// Generates vector embeddings from code and queries.
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError>;

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError>;

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError>;

    /// `false` when this service cannot produce embeddings at all
    /// (`--no-embeddings` mode).  Indexing consults this to skip the embed
    /// stage and store chunks without vectors instead of calling the embed
    /// methods (which such services reject with an error).
    fn embeddings_enabled(&self) -> bool {
        true
    }

    fn config(&self) -> &EmbeddingConfig;
}
