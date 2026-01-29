use std::sync::Arc;

use tracing::{debug, info};

use crate::domain::{DomainError, EmbeddingService, SearchQuery, SearchResult, VectorRepository};

pub struct SearchCodeUseCase {
    vector_repo: Arc<dyn VectorRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl SearchCodeUseCase {
    pub fn new(
        vector_repo: Arc<dyn VectorRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            vector_repo,
            embedding_service,
        }
    }

    pub async fn execute(&self, query: SearchQuery) -> Result<Vec<SearchResult>, DomainError> {
        info!("Searching for: {}", query.query);

        let query_embedding = self.embedding_service.embed_query(&query.query).await?;

        debug!(
            "Generated query embedding with {} dimensions",
            query_embedding.len()
        );

        let results = self.vector_repo.search(&query_embedding, &query).await?;

        info!("Found {} results", results.len());

        Ok(results)
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, DomainError> {
        let search_query = SearchQuery::new(query).with_limit(limit);
        self.execute(search_query).await
    }
}
