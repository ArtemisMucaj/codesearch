use std::time::Duration;

use async_trait::async_trait;
use openai_rs::{EmbeddingClient, Endpoint, OpenAiEmbeddingClient};
use tracing::{debug, warn};

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
/// Request timeout preserved from the pre-crate adapter (the crate's `Endpoint`
/// default is far higher, tuned for slow chat completions rather than embeddings).
const EMBEDDING_TIMEOUT_SECS: u64 = 60;

/// HTTP embedding adapter targeting the OpenAI-compatible `/v1/embeddings`
/// endpoint — e.g. LM Studio running locally. The HTTP protocol (batching and
/// L2-normalisation, both on by default) is delegated to
/// [`openai_rs::OpenAiEmbeddingClient`]; this adapter keeps the
/// [`EmbeddingService`] port, the [`EmbeddingConfig`], and the dimension-mismatch
/// warning (the crate has no expected width to check against).
///
/// **Configuration**:
/// - Base URL: `OPENAI_BASE_URL` env var (default `http://localhost:1234`).
/// - Model name and dimensions: supplied at construction time from `--embedding-model`
///   and `--embedding-dimensions` CLI flags; they are stored in `namespace_config`
///   and validated on every subsequent open.
pub struct OpenAiEmbedding {
    client: OpenAiEmbeddingClient,
    config: EmbeddingConfig,
}

impl OpenAiEmbedding {
    /// `model` — the model name sent in every `/v1/embeddings` request (must
    /// match the model loaded in the target server).
    ///
    /// `dimensions` — the number of dimensions the model outputs; must match the
    /// value stored in `namespace_config` for the target namespace (enforced by
    /// the vector repository on open).
    pub fn new(model: impl Into<String>, dimensions: usize) -> Result<Self, DomainError> {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = model.into();

        debug!(
            "OpenAiEmbedding: base={}, model={}, dims={}",
            base, model, dimensions
        );

        let endpoint =
            Endpoint::new(base).with_timeout(Duration::from_secs(EMBEDDING_TIMEOUT_SECS));
        // Building the client is fallible (a malformed `OPENAI_BASE_URL` is the
        // usual cause), so propagate rather than abort the process.
        let client =
            OpenAiEmbeddingClient::new(&endpoint, model.clone()).map_err(super::map_openai_err)?;

        Ok(Self {
            client,
            config: EmbeddingConfig::new(model, dimensions, 512),
        })
    }

    /// Embed `texts`, returning one vector per input (batched and L2-normalised
    /// by the crate). Emits a warning if the model's output width does not match
    /// the configured dimensions, then returns the vectors as-is.
    async fn embed_texts(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, DomainError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let n = texts.len();
        let embeddings = self
            .client
            .embed_batch(&texts)
            .await
            .map_err(super::map_openai_err)?;

        let expected = self.config.dimensions();
        if let Some(width) = embeddings.first().map(|v| v.len()) {
            if width != expected {
                warn!(
                    "OpenAiEmbedding: model '{}' returned {} dimensions, expected {}. \
                     Check that the model matches --embedding-model and \
                     --embedding-dimensions.",
                    self.config.model_name(),
                    width,
                    expected
                );
            }
        }

        debug!(
            "OpenAiEmbedding: {} embedding(s) ({}-dim)",
            n,
            embeddings.first().map(|v| v.len()).unwrap_or(0)
        );

        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingService for OpenAiEmbedding {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        let text = format!(
            "{} {}",
            chunk.qualified_name().as_deref().unwrap_or(""),
            chunk.content()
        );
        let vectors = self.embed_texts(vec![text]).await?;
        Ok(Embedding::new(
            chunk.id().to_string(),
            vectors.into_iter().next().unwrap_or_default(),
            self.config.model_name().to_string(),
        ))
    }

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        let texts: Vec<String> = chunks
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    c.qualified_name().as_deref().unwrap_or(""),
                    c.content()
                )
            })
            .collect();

        let vectors = self.embed_texts(texts).await?;

        Ok(chunks
            .iter()
            .zip(vectors)
            .map(|(chunk, vector)| {
                Embedding::new(
                    chunk.id().to_string(),
                    vector,
                    self.config.model_name().to_string(),
                )
            })
            .collect())
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        let vectors = self.embed_texts(vec![query.to_string()]).await?;
        vectors
            .into_iter()
            .next()
            .ok_or_else(|| DomainError::internal("OpenAiEmbedding: empty response for query"))
    }

    fn config(&self) -> &EmbeddingConfig {
        &self.config
    }
}
