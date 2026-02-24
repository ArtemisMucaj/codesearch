use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::debug;

use crate::application::{rrf_fuse, VectorRepository};
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

    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError> {
        let mut chunk_store = self.chunks.lock().await;
        let mut embedding_store = self.embeddings.lock().await;

        let ids: Vec<String> = chunk_store
            .values()
            .filter(|chunk| {
                chunk.repository_id() == repository_id && chunk.file_path() == file_path
            })
            .map(|chunk| chunk.id().to_string())
            .collect();

        let count = ids.len() as u64;
        for id in ids {
            chunk_store.remove(&id);
            embedding_store.remove(&id);
        }

        Ok(count)
    }

    async fn search(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let fetch_limit = if query.is_text_search() {
            (query.limit() * 2).max(20)
        } else {
            query.limit()
        };

        let semantic = self.search_semantic(query_embedding, query, fetch_limit).await;

        if !query.is_text_search() {
            return Ok(semantic);
        }

        let terms: Vec<&str> = query.query().split_whitespace().collect();
        let text = self.search_text(&terms, query, fetch_limit).await;

        let mut fused = rrf_fuse(vec![semantic, text], query.limit());
        if let Some(min) = query.min_score() {
            fused.retain(|r| r.score() >= min);
        }
        Ok(fused)
    }

    async fn count(&self) -> Result<u64, DomainError> {
        let chunks = self.chunks.lock().await;
        Ok(chunks.len() as u64)
    }
}

impl InMemoryVectorRepository {
    async fn search_semantic(
        &self,
        query_embedding: &[f32],
        query: &SearchQuery,
        limit: usize,
    ) -> Vec<SearchResult> {
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
            if results.len() >= limit {
                break;
            }
            // In hybrid mode the full candidate pool is passed to rrf_fuse, so
            // min_score is applied there; only filter early in semantic-only mode.
            if !query.is_text_search() {
                if let Some(min) = query.min_score() {
                    if score < min {
                        continue;
                    }
                }
            }
            let chunk = match chunk_store.get(&chunk_id) {
                Some(c) => c.clone(),
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

        results
    }

    async fn search_text(&self, terms: &[&str], query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        if terms.is_empty() {
            return vec![];
        }

        let chunk_store = self.chunks.lock().await;
        let max_score = (terms.len() * 3) as f32;

        let mut results: Vec<SearchResult> = chunk_store
            .values()
            .filter_map(|chunk| {
                let content_lower = chunk.content().to_lowercase();
                let symbol_lower = chunk
                    .symbol_name()
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();

                let score: f32 = terms
                    .iter()
                    .map(|t| {
                        let t = t.to_lowercase();
                        let c = if content_lower.contains(&t) { 1.0_f32 } else { 0.0 };
                        let s = if symbol_lower.contains(&t) { 2.0_f32 } else { 0.0 };
                        c + s
                    })
                    .sum::<f32>()
                    / max_score;

                if score == 0.0 {
                    return None;
                }
                if let Some(langs) = query.languages() {
                    if !langs.iter().any(|l| l == chunk.language().as_str()) {
                        return None;
                    }
                }
                if let Some(node_types) = query.node_types() {
                    if !node_types.iter().any(|nt| nt == chunk.node_type().as_str()) {
                        return None;
                    }
                }
                if let Some(repos) = query.repository_ids() {
                    if !repos.contains(&chunk.repository_id().to_string()) {
                        return None;
                    }
                }
                Some(SearchResult::new(chunk.clone(), score))
            })
            .collect();

        results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::application::VectorRepository;
    use crate::domain::{CodeChunk, Embedding, Language, NodeType, SearchQuery};

    /// Build a unit vector of `dims` dimensions pointing along `axis`.
    fn unit_vec(dims: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dims];
        v[axis] = 1.0;
        v
    }

    fn make_chunk(id: &str, content: &str, symbol: Option<&str>) -> CodeChunk {
        CodeChunk::reconstitute(
            id.to_string(),
            "file.rs".to_string(),
            content.to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            symbol.map(|s| s.to_string()),
            None,
            "repo".to_string(),
        )
    }

    /// Seed a repository with two chunks:
    ///   "alpha" – embedding along dim 0, content + symbol contain "alpha"
    ///   "beta"  – embedding along dim 1, content + symbol contain "beta"
    async fn seeded_repo() -> Arc<InMemoryVectorRepository> {
        let alpha = make_chunk("chunk-alpha", "fn alpha() { alpha() }", Some("alpha"));
        let beta = make_chunk("chunk-beta", "fn beta() { beta() }", Some("beta"));

        let alpha_emb = Embedding::new("chunk-alpha".to_string(), unit_vec(4, 0), "test".to_string());
        let beta_emb = Embedding::new("chunk-beta".to_string(), unit_vec(4, 1), "test".to_string());

        let repo = Arc::new(InMemoryVectorRepository::new());
        repo.save_batch(&[alpha, beta], &[alpha_emb, beta_emb])
            .await
            .unwrap();
        repo
    }

    #[tokio::test]
    async fn semantic_only_mode_returns_cosine_scores() {
        let repo = seeded_repo().await;
        // Query points along dim 0 → cosine similarity = 1.0 for alpha, 0.0 for beta.
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("alpha").with_limit(5);
        // text_search is false by default
        assert!(!query.is_text_search());

        let results = repo.search(&query_embedding, &query).await.unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].chunk().id(), "chunk-alpha");
        // Cosine-based scores are in [0, 1]; much larger than RRF scores (~0.016)
        assert!(results[0].score() > 0.5, "expected cosine similarity score");
    }

    #[tokio::test]
    async fn hybrid_mode_produces_rrf_scores() {
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("alpha").with_limit(5).with_text_search(true);

        let results = repo.search(&query_embedding, &query).await.unwrap();

        assert!(!results.is_empty());
        // RRF scores are always < 1/(RRF_K+1) * 2 ≈ 0.033
        for r in &results {
            assert!(
                r.score() < 0.1,
                "expected RRF score, got {:.4}",
                r.score()
            );
        }
    }

    #[tokio::test]
    async fn item_in_both_legs_outranks_item_in_one_leg() {
        // "alpha" is rank 1 in semantic (cosine sim = 1.0) AND rank 1 in text
        //   (content + symbol both contain "alpha" → max text score).
        // "beta" is rank 2 in semantic (cosine sim = 0.0, excluded) and absent from text.
        // After fusion "alpha" must be ranked first.
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("alpha").with_limit(5).with_text_search(true);

        let results = repo.search(&query_embedding, &query).await.unwrap();

        assert_eq!(
            results[0].chunk().id(),
            "chunk-alpha",
            "item in both legs should rank first"
        );
    }

    #[tokio::test]
    async fn min_score_filters_fused_results() {
        // RRF scores are ~0.016–0.033; a min_score of 0.5 must remove all results.
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("alpha")
            .with_limit(5)
            .with_text_search(true)
            .with_min_score(0.5);

        let results = repo.search(&query_embedding, &query).await.unwrap();

        assert!(
            results.is_empty(),
            "RRF scores should all fall below min_score=0.5"
        );
    }

    #[tokio::test]
    async fn min_score_not_applied_early_in_semantic_leg_during_hybrid() {
        // In hybrid mode, min_score must not prune semantic candidates before
        // rrf_fuse; otherwise the alpha chunk (cosine sim = 1.0) would be kept
        // but beta (cosine sim = 0.0) dropped, producing an asymmetric pool.
        // We verify that both candidates can still reach the fusion stage.
        // Use a min_score deliberately lower than any RRF score so results survive.
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        // "alpha beta" hits both chunks in the text leg; both should survive fusion.
        let query = SearchQuery::new("alpha beta")
            .with_limit(5)
            .with_text_search(true)
            .with_min_score(0.001); // below all RRF scores

        let results = repo.search(&query_embedding, &query).await.unwrap();

        let ids: Vec<&str> = results.iter().map(|r| r.chunk().id()).collect();
        assert!(ids.contains(&"chunk-alpha"), "alpha should survive fusion");
        assert!(ids.contains(&"chunk-beta"), "beta should survive fusion");
    }

    #[tokio::test]
    async fn empty_query_falls_back_to_semantic() {
        // Whitespace-only query splits into zero terms → text leg returns empty →
        // fused result equals semantic-only result.
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("   ").with_limit(5).with_text_search(true);

        let results = repo.search(&query_embedding, &query).await.unwrap();

        // Should still return results from the semantic leg via rrf_fuse
        assert!(!results.is_empty(), "semantic leg should provide fallback results");
    }

    #[tokio::test]
    async fn limit_is_respected_in_hybrid_mode() {
        let repo = seeded_repo().await;
        let query_embedding = unit_vec(4, 0);
        let query = SearchQuery::new("alpha beta")
            .with_limit(1)
            .with_text_search(true);

        let results = repo.search(&query_embedding, &query).await.unwrap();

        assert!(results.len() <= 1, "limit should cap fused results");
    }
}
