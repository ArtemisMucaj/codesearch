use async_trait::async_trait;

use crate::domain::{DomainError, SearchResult};

/// Reranks search results based on query relevance using a cross-encoder model.
#[async_trait]
pub trait RerankingService: Send + Sync {
    /// Rerank a list of search results based on the query.
    /// Returns results sorted by relevance score (highest first).
    async fn rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
        top_k: Option<usize>,
    ) -> Result<Vec<SearchResult>, DomainError>;

    /// Get the model name used for reranking
    fn model_name(&self) -> &str;
}
