use std::sync::Arc;

use tracing::{debug, info};

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

        let query_embedding = self.embedding_service.embed_query(query.query()).await?;

        debug!(
            "Generated query embedding with {} dimensions",
            query_embedding.len()
        );

        let fetch_limit = if self.reranking_service.is_some() {
            if query.limit() <= 10 {
                100
            } else {
                query.limit() * 10
            }
        } else {
            query.limit()
        };

        let search_query = if fetch_limit != query.limit() {
            query.clone().with_limit(fetch_limit)
        } else {
            query.clone()
        };

        let mut results = self.vector_repo.search(&query_embedding, &search_query).await?;

        if let Some(ref reranker) = self.reranking_service {
            info!(
                "Reranking {} candidates with {}",
                results.len(),
                reranker.model_name()
            );

            results = reranker
                .rerank(query.query(), results, Some(query.limit()))
                .await?;
        }

        info!("Found {} results", results.len());

        Ok(results)
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, DomainError> {
        let search_query = SearchQuery::new(query).with_limit(limit);
        self.execute(search_query).await
    }
}
