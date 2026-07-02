use async_trait::async_trait;

use crate::application::EmbeddingService;
use crate::connector::adapter::NO_EMBEDDINGS_MODEL;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

/// Placeholder max sequence length; never used for inference since this
/// service refuses to embed.
const NO_EMBEDDING_MAX_SEQ: usize = 512;

/// Embedding service used in `--no-embeddings` mode.
///
/// Indexing skips the embed stage entirely and search skips query embedding
/// when the store has no vectors, so none of these methods should ever be
/// reached.  Each returns an error rather than a fake vector so that any
/// code path that would silently produce meaningless embeddings fails loudly
/// instead.
pub struct NoEmbedding {
    config: EmbeddingConfig,
}

impl NoEmbedding {
    pub fn new(dimensions: usize) -> Self {
        Self {
            config: EmbeddingConfig::new(
                NO_EMBEDDINGS_MODEL.to_string(),
                dimensions,
                NO_EMBEDDING_MAX_SEQ,
            ),
        }
    }
}

#[async_trait]
impl EmbeddingService for NoEmbedding {
    async fn embed_chunk(&self, _chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        Err(DomainError::invalid_input(
            "Embeddings are disabled (--no-embeddings); cannot embed a chunk".to_string(),
        ))
    }

    async fn embed_chunks(&self, _chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError> {
        Err(DomainError::invalid_input(
            "Embeddings are disabled (--no-embeddings); cannot embed chunks".to_string(),
        ))
    }

    async fn embed_query(&self, _query: &str) -> Result<Vec<f32>, DomainError> {
        Err(DomainError::invalid_input(
            "Embeddings are disabled (--no-embeddings); cannot embed a query".to_string(),
        ))
    }

    fn config(&self) -> &EmbeddingConfig {
        &self.config
    }
}
