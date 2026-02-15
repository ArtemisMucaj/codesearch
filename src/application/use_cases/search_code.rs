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
        info!("Searching for: {}", query.query());

        let start_time = Instant::now();

        let query_embedding = self.embedding_service.embed_query(query.query()).await?;

        debug!(
            "Generated query embedding with {} dimensions",
            query_embedding.len()
        );

        let fetch_limit = if self.reranking_service.is_some() {
            // Use a log-based formula to scale candidates for reranking:
            // fetch_limit = num + num * ceil(ln(num))
            // This gives enough candidates without being overwhelming:
            //   num=10  -> 10 + 10*3 = 40
            //   num=20  -> 20 + 20*3 = 80
            //   num=50  -> 50 + 50*4 = 250
            //   num=100 -> 100 + 100*5 = 600
            // Default to 20 candidates when not specified (i.e. when limit <= 10)
            let base = if query.limit() <= 10 { 20 } else { query.limit() };
            let log_factor = (base as f64).ln().ceil() as usize;
            base + base * log_factor
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

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let search_query = SearchQuery::new(query).with_limit(limit);
        self.execute(search_query).await
    }
}
