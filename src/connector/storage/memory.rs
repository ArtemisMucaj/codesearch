//! In-memory embedding storage.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::debug;

use crate::domain::{
    ChunkRepository, DomainError, Embedding, EmbeddingRepository, SearchQuery, SearchResult,
};

/// In-memory embedding storage for testing and development.
pub struct InMemoryEmbeddingStorage {
    embeddings: Arc<Mutex<HashMap<String, Embedding>>>,
    chunk_repo: Arc<dyn ChunkRepository>,
}

impl InMemoryEmbeddingStorage {
    pub fn new(chunk_repo: Arc<dyn ChunkRepository>) -> Self {
        Self {
            embeddings: Arc::new(Mutex::new(HashMap::new())),
            chunk_repo,
        }
    }
}

#[async_trait]
impl EmbeddingRepository for InMemoryEmbeddingStorage {
    async fn save(&self, embedding: &Embedding) -> Result<(), DomainError> {
        let mut embeddings = self.embeddings.lock().await;
        embeddings.insert(embedding.chunk_id.clone(), embedding.clone());
        Ok(())
    }

    async fn save_batch(&self, embeddings: &[Embedding]) -> Result<(), DomainError> {
        let mut storage = self.embeddings.lock().await;
        for embedding in embeddings {
            storage.insert(embedding.chunk_id.clone(), embedding.clone());
        }
        debug!("Saved {} embeddings to memory", embeddings.len());
        Ok(())
    }

    async fn find_by_chunk_id(&self, chunk_id: &str) -> Result<Option<Embedding>, DomainError> {
        let embeddings = self.embeddings.lock().await;
        Ok(embeddings.get(chunk_id).cloned())
    }

    async fn delete(&self, chunk_id: &str) -> Result<(), DomainError> {
        let mut embeddings = self.embeddings.lock().await;
        embeddings.remove(chunk_id);
        Ok(())
    }

    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError> {
        let chunks = self.chunk_repo.find_by_repository(repository_id).await?;
        let mut embeddings = self.embeddings.lock().await;
        for chunk in chunks {
            embeddings.remove(&chunk.id);
        }
        Ok(())
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let embeddings = self.embeddings.lock().await;

        let mut results: Vec<(String, f32)> = embeddings
            .values()
            .map(|e| {
                let score = cosine_similarity(query_embedding, &e.vector);
                (e.chunk_id.clone(), score)
            })
            .collect();

        // Sort by score descending
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Take top results
        results.truncate(query.limit);

        let mut search_results = Vec::new();
        for (chunk_id, score) in results {
            if let Some(min_score) = query.min_score {
                if score < min_score {
                    continue;
                }
            }

            if let Some(chunk) = self.chunk_repo.find_by_id(&chunk_id).await? {
                // Apply language filter
                if let Some(ref languages) = query.languages {
                    if !languages.iter().any(|l| l == chunk.language.as_str()) {
                        continue;
                    }
                }

                // Apply node type filter
                if let Some(ref node_types) = query.node_types {
                    if !node_types.iter().any(|t| t == chunk.node_type.as_str()) {
                        continue;
                    }
                }

                // Apply repository filter
                if let Some(ref repo_ids) = query.repository_ids {
                    if !repo_ids.contains(&chunk.repository_id) {
                        continue;
                    }
                }

                search_results.push(SearchResult::new(chunk, score));
            }
        }

        Ok(search_results)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let embeddings = self.embeddings.lock().await;
        Ok(embeddings.len() as u64)
    }
}

/// Calculate cosine similarity between two vectors.
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
