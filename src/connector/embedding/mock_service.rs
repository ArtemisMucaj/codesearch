use async_trait::async_trait;
use rand::SeedableRng;
use rand::Rng;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tracing::debug;

use crate::domain::{CodeChunk, DomainError, Embedding, EmbeddingConfig, EmbeddingService};

pub struct MockEmbeddingService {
    config: EmbeddingConfig,
}

impl MockEmbeddingService {
    pub fn new() -> Self {
        Self {
            config: EmbeddingConfig {
                model_name: "mock-embedding".to_string(),
                dimensions: 384,
                max_sequence_length: 512,
            },
        }
    }

    pub fn with_dimensions(dimensions: usize) -> Self {
        Self {
            config: EmbeddingConfig {
                model_name: "mock-embedding".to_string(),
                dimensions,
                max_sequence_length: 512,
            },
        }
    }

    fn generate_embedding(&self, text: &str) -> Vec<f32> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let seed = hasher.finish();

        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut vector: Vec<f32> = (0..self.config.dimensions)
            .map(|_| rng.gen_range(-1.0..1.0))
            .collect();

        let magnitude: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for x in &mut vector {
                *x /= magnitude;
            }
        }

        vector
    }

    fn prepare_text(chunk: &CodeChunk) -> String {
        let mut text = String::new();

        if let Some(ref name) = chunk.symbol_name {
            text.push_str(&format!("{} ", name));
        }

        text.push_str(&format!("[{}] ", chunk.node_type));
        text.push_str(&chunk.content);

        text
    }
}

impl Default for MockEmbeddingService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingService for MockEmbeddingService {
    async fn embed_chunk(&self, chunk: &CodeChunk) -> Result<Embedding, DomainError> {
        let text = Self::prepare_text(chunk);
        let vector = self.generate_embedding(&text);

        debug!(
            "Generated mock embedding for chunk {} with {} dimensions",
            chunk.id,
            vector.len()
        );

        Ok(Embedding::new(
            chunk.id.clone(),
            vector,
            self.config.model_name.clone(),
        ))
    }

    async fn embed_chunks(&self, chunks: &[CodeChunk]) -> Result<Vec<Embedding>, DomainError> {
        let results: Vec<Embedding> = chunks
            .iter()
            .map(|chunk| {
                let text = Self::prepare_text(chunk);
                let vector = self.generate_embedding(&text);
                Embedding::new(chunk.id.clone(), vector, self.config.model_name.clone())
            })
            .collect();

        debug!("Generated {} mock embeddings", results.len());

        Ok(results)
    }

    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, DomainError> {
        Ok(self.generate_embedding(query))
    }

    fn config(&self) -> &EmbeddingConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_embedding_consistency() {
        let service = MockEmbeddingService::new();

        let embedding1 = service.embed_query("hello world").await.unwrap();
        let embedding2 = service.embed_query("hello world").await.unwrap();

        assert_eq!(embedding1, embedding2);
    }

    #[tokio::test]
    async fn test_mock_embedding_dimensions() {
        let service = MockEmbeddingService::with_dimensions(128);

        let embedding = service.embed_query("test").await.unwrap();

        assert_eq!(embedding.len(), 128);
    }

    #[tokio::test]
    async fn test_mock_embedding_normalized() {
        let service = MockEmbeddingService::new();

        let embedding = service.embed_query("test").await.unwrap();
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

        assert!((magnitude - 1.0).abs() < 0.001);
    }
}
