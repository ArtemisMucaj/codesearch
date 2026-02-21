use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::application::{EmbeddingService, RerankingService, VectorRepository};
use crate::domain::{DomainError, SearchQuery, SearchResult};

pub struct SearchCodeUseCase {
    vector_repo: Arc<dyn VectorRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
}

impl SearchCodeUseCase {
    pub fn new(
        vector_repo: Arc<dyn VectorRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            vector_repo,
            embedding_service,
            reranking_service: None,
        }
    }

    pub fn with_reranking(mut self, service: Arc<dyn RerankingService>) -> Self {
        self.reranking_service = Some(service);
        self
    }

    pub async fn execute(&self, query: SearchQuery) -> Result<Vec<SearchResult>, DomainError> {
        if query.is_hybrid() {
            return self.execute_hybrid(query).await;
        }

        info!("Searching for: {}", query.query());

        let start_time = Instant::now();

        let query_embedding = self.embedding_service.embed_query(query.query()).await?;

        debug!(
            "Generated query embedding with {} dimensions",
            query_embedding.len()
        );

        let fetch_limit = if self.reranking_service.is_some() {
            // Use an inverse-log formula so the overhead shrinks as num grows:
            // fetch_limit = num + ceil(num / ln(num))
            //   num=20 (default) -> 20 + 7  = 27  (+35%)
            //   num=50           -> 50 + 13  = 63  (+26%)
            //   num=100          -> 100 + 22 = 122 (+22%)
            // Default to 20 base candidates when not specified (i.e. when limit <= 10)
            let base = if query.limit() <= 10 { 20 } else { query.limit() };
            let extra = ((base as f64) / (base as f64).ln()).ceil() as usize;
            base + extra
        } else {
            query.limit()
        };

        let search_query = if fetch_limit != query.limit() {
            query.clone().with_limit(fetch_limit)
        } else {
            query.clone()
        };

        let mut results = self
            .vector_repo
            .search(&query_embedding, &search_query)
            .await?;

        if let Some(ref reranker) = self.reranking_service {
            // Filter out very low-scoring results before reranking — they are
            // unlikely to resurface and just slow down the cross-encoder.
            let before_filter = results.len();
            results.retain(|r| r.score() >= 0.1);
            let filtered = before_filter - results.len();
            if filtered > 0 {
                warn!(
                    "Excluded {} candidates with score < 0.1 before reranking",
                    filtered
                );
            }

            info!(
                "Reranking {} candidates with {}",
                results.len(),
                reranker.model_name()
            );

            results = reranker
                .rerank(query.query(), results, Some(query.limit()))
                .await?;
        }

        let duration = start_time.elapsed();
        info!(
            "Found {} results in {:.2}s",
            results.len(),
            duration.as_secs_f64()
        );

        Ok(results)
    }

    /// Hybrid search: run semantic + keyword searches in parallel then fuse with RRF.
    async fn execute_hybrid(&self, query: SearchQuery) -> Result<Vec<SearchResult>, DomainError> {
        info!("Hybrid searching for: {}", query.query());
        let start_time = Instant::now();

        let query_embedding = self.embedding_service.embed_query(query.query()).await?;

        // Fetch extra candidates from both legs so RRF has a meaningful pool.
        let fetch_limit = (query.limit() * 2).max(20);
        let fetch_query = query.clone().with_limit(fetch_limit);

        let terms: Vec<&str> = query.query().split_whitespace().collect();

        let (semantic_results, text_results) = tokio::join!(
            self.vector_repo.search(&query_embedding, &fetch_query),
            self.vector_repo.search_text(&terms, &fetch_query),
        );

        let semantic_results = semantic_results?;
        let text_results = text_results?;

        debug!(
            "Hybrid: {} semantic + {} text candidates",
            semantic_results.len(),
            text_results.len()
        );

        let mut fused = Self::rrf_fuse(semantic_results, text_results, query.limit());

        if let Some(ref reranker) = self.reranking_service {
            fused = reranker
                .rerank(query.query(), fused, Some(query.limit()))
                .await?;
        }

        let duration = start_time.elapsed();
        info!(
            "Hybrid found {} results in {:.2}s",
            fused.len(),
            duration.as_secs_f64()
        );

        Ok(fused)
    }

    /// Reciprocal Rank Fusion (k=60) over two ranked result lists.
    /// Each result's score = 1/(60 + rank_semantic) + 1/(60 + rank_text),
    /// using 0 for the leg that didn't return the chunk.
    fn rrf_fuse(
        semantic: Vec<SearchResult>,
        text: Vec<SearchResult>,
        limit: usize,
    ) -> Vec<SearchResult> {
        const K: f32 = 60.0;
        // chunk_id → (SearchResult, rrf_score)
        let mut scores: HashMap<String, (SearchResult, f32)> = HashMap::new();

        for (rank, result) in semantic.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f32);
            let id = result.chunk().id().to_string();
            scores
                .entry(id)
                .and_modify(|(_, s)| *s += rrf)
                .or_insert((result, rrf));
        }

        for (rank, result) in text.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f32);
            let id = result.chunk().id().to_string();
            scores
                .entry(id)
                .and_modify(|(_, s)| *s += rrf)
                .or_insert((result, rrf));
        }

        let mut fused: Vec<(SearchResult, f32)> = scores.into_values().collect();
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        fused
            .into_iter()
            .take(limit)
            .map(|(r, rrf_score)| SearchResult::new(r.chunk().clone(), rrf_score))
            .collect()
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let search_query = SearchQuery::new(query).with_limit(limit);
        self.execute(search_query).await
    }
}
