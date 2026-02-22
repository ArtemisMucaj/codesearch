use async_trait::async_trait;

use crate::domain::DomainError;

/// Expands a natural language query into multiple semantically related variants
/// to improve search recall through multi-query retrieval.
///
/// Each variant is phrased differently so that the embedding model produces
/// complementary vectors, surfacing code that a single query phrasing might miss.
/// Results from all variants are later fused via Reciprocal Rank Fusion.
#[async_trait]
pub trait QueryExpander: Send + Sync {
    /// Expand a query into multiple variants.
    ///
    /// The original query is always included as the first element.
    /// Returns at least one element (the original query) even when expansion
    /// produces no useful additional variants.
    async fn expand(&self, query: &str) -> Result<Vec<String>, DomainError>;
}
