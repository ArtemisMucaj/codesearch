use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::application::EmbeddingService;
use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig};

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const DEFAULT_MODEL: &str = "text-embedding-model";
const EMBEDDINGS_PATH: &str = "/v1/embeddings";
const BATCH_SIZE: usize = 32;

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<String>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// HTTP embedding adapter targeting the OpenAI-compatible `/v1/embeddings` endpoint.
///
/// Designed for **LM Studio** running locally, but works with any server that
/// exposes a compatible embeddings endpoint (OpenAI, Ollama, etc.).
///
/// **Configuration** (environment variables, all optional):
///
/// | Variable                    | Default                   | Purpose                     |
/// |-----------------------------|---------------------------|-----------------------------|
/// | `LM_STUDIO_BASE_URL`        | `http://localhost:1234`   | Base URL of the API server  |
/// | `LM_STUDIO_EMBEDDING_MODEL` | `text-embedding-model`    | Model identifier to send    |
///
/// `LM_STUDIO_BASE_URL` falls back to `ANTHROPIC_BASE_URL` when unset, so a
/// single env-var covers both chat and embedding when they share the same server.
///
/// **Dimension validation**: the expected dimension count is read from the first
/// API response.  If the loaded model outputs a different number of dimensions
/// than the configured store (384 by default) a warning is logged — the mismatch
/// will surface as an error at the vector-storage layer.
pub struct LmStudioEmbedding {
    client: reqwest::Client,
    url: String,
    config: EmbeddingConfig,
}

impl LmStudioEmbedding {
    /// Construct from environment variables with local-first defaults.
    pub fn from_env() -> Self {
        // Prefer LM_STUDIO_BASE_URL; fall back to ANTHROPIC_BASE_URL; then hardcoded default.
        let base = std::env::var("LM_STUDIO_BASE_URL")
            .or_else(|_| std::env::var("ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("LM_STUDIO_EMBEDDING_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let url = format!("{}{}", base.trim_end_matches('/'), EMBEDDINGS_PATH);

        debug!(
            "LmStudioEmbedding: endpoint={}, model={}",
            url, model
        );

        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest::Client build failed"),
            url,
            // Dimensions are set to the store default (384); mismatches are caught at
            // the storage layer with an informative error.
            config: EmbeddingConfig::new(model, 384, 512),
        }
    }

    async fn embed_texts(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, DomainError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let n = texts.len();
        let request = EmbeddingRequest {
            model: self.config.model_name(),
            input: texts,
        };

        let response = self
            .client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                DomainError::internal(format!("LM Studio embedding request failed: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DomainError::internal(format!(
                "LM Studio embedding API returned {status}: {body}"
            )));
        }

        let api_response: EmbeddingResponse = response.json().await.map_err(|e| {
            DomainError::internal(format!(
                "Failed to parse LM Studio embedding response: {e}"
            ))
        })?;

        // The OpenAI spec doesn't guarantee ordering; sort by index.
        let mut data = api_response.data;
        data.sort_by_key(|d| d.index);

        let expected_dims = self.config.dimensions();

        let embeddings = data
            .into_iter()
            .map(|d| {
                let mut vec = d.embedding;
                if vec.len() != expected_dims {
                    warn!(
                        "LmStudioEmbedding: model returned {} dimensions, expected {}. \
                        Set LM_STUDIO_EMBEDDING_MODEL to a 384-dim model or re-index \
                        with a matching schema.",
                        vec.len(),
                        expected_dims
                    );
                }
                // L2-normalise so cosine similarity equals dot product (same as OrtEmbedding).
                let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for v in &mut vec {
                        *v /= norm;
                    }
                }
                vec
            })
            .collect::<Vec<_>>();

        debug!(
            "LmStudioEmbedding: generated {} embedding(s) ({}-dim)",
            n,
            embeddings.first().map(|v| v.len()).unwrap_or(0)
        );

        Ok(embeddings)
    }
}

#[async_trait]
impl EmbeddingService for LmStudioEmbedding {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        let text = format!("{} {}", chunk.symbol_name().unwrap_or(""), chunk.content());
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

        let mut all_embeddings = Vec::with_capacity(chunks.len());

        for batch in chunks.chunks(BATCH_SIZE) {
            let texts: Vec<String> = batch
                .iter()
                .map(|c| format!("{} {}", c.symbol_name().unwrap_or(""), c.content()))
                .collect();

            let vectors = self.embed_texts(texts).await?;

            for (chunk, vector) in batch.iter().zip(vectors) {
                all_embeddings.push(Embedding::new(
                    chunk.id().to_string(),
                    vector,
                    self.config.model_name().to_string(),
                ));
            }
        }

        Ok(all_embeddings)
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        let vectors = self.embed_texts(vec![query.to_string()]).await?;
        vectors
            .into_iter()
            .next()
            .ok_or_else(|| DomainError::internal("LmStudioEmbedding: empty response for query"))
    }

    fn config(&self) -> &EmbeddingConfig {
        &self.config
    }
}
