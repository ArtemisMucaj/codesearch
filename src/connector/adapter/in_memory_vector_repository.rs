use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::VectorRepository;
use crate::domain::{CodeChunk, DomainError, Embedding, SearchQuery, SearchResult};

pub struct InMemoryVectorRepository {
    chunks: Arc<Mutex<HashMap<String, CodeChunk>>>,
    embeddings: Arc<Mutex<HashMap<String, Embedding>>>,
}

impl InMemoryVectorRepository {
    pub fn new() -> Self {
        Self {
            chunks: Arc::new(Mutex::new(HashMap::new())),
            embeddings: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryVectorRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VectorRepository for InMemoryVectorRepository {
    async fn save_batch(
        &self,
        chunks: &[CodeChunk],
        embeddings: &[Embedding],
    ) -> Result<(), DomainError> {
        let mut chunk_store = self.chunks.lock().await;
        let mut embedding_store = self.embeddings.lock().await;

        for chunk in chunks {
            chunk_store.insert(chunk.id().to_string(), chunk.clone());
        }

        for embedding in embeddings {
            embedding_store.insert(embedding.chunk_id().to_string(), embedding.clone());
        }

        debug!(
            "Saved {} chunks and {} embeddings to memory",
            chunks.len(),
            embeddings.len()
        );
        Ok(())
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let mut chunk_store = self.chunks.lock().await;
        let mut embedding_store = self.embeddings.lock().await;
        chunk_store.remove(chunk_id);
        embedding_store.remove(chunk_id);
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let mut chunk_store = self.chunks.lock().await;
        let mut embedding_store = self.embeddings.lock().await;

        let ids: Vec<String> = chunk_store
            .values()
            .filter(|chunk| chunk.repository_id() == repository_id)
            .map(|chunk| chunk.id().to_string())
            .collect();

        for id in ids {
            chunk_store.remove(&id);
            embedding_store.remove(&id);
        }

        Ok(())
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let scored_ids: Vec<(String, f32)> = {
            let embeddings = self.embeddings.lock().await;
            let mut scored: Vec<(String, f32)> = embeddings
                .values()
                .map(|embedding| {
                    let score = cosine_similarity(query_embedding, embedding.vector());
                    (embedding.chunk_id().to_string(), score)
                })
                .collect();

            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored
        };

        let chunk_store = self.chunks.lock().await;
        let mut results = Vec::new();

        for (chunk_id, score) in scored_ids {
            if results.len() >= query.limit() {
                break;
            }

            if let Some(min_score) = query.min_score() {
                if score < min_score {
                    continue;
                }
            }

            let chunk = match chunk_store.get(&chunk_id) {
                Some(chunk) => chunk.clone(),
                None => continue,
            };

            if let Some(languages) = query.languages() {
                if !languages.iter().any(|l| l == chunk.language().as_str()) {
                    continue;
                }
            }

            if let Some(node_types) = query.node_types() {
                if !node_types.iter().any(|t| t == chunk.node_type().as_str()) {
                    continue;
                }
            }

            if let Some(repo_ids) = query.repository_ids() {
                if !repo_ids.contains(&chunk.repository_id().to_string()) {
                    continue;
                }
            }

            results.push(SearchResult::new(chunk, score));
        }

        Ok(results)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let chunks = self.chunks.lock().await;
        Ok(chunks.len() as u64)
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}
