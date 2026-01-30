use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::application::RerankingService;
use crate::domain::{DomainError, SearchResult};

pub struct MockReranking;

impl MockReranking {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockReranking {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RerankingService for MockReranking {
    async fn rerank(
        &self,
        query: &str,
        results: Vec<SearchResult>,
        top_k: Option<usize>,
    ) -> Result<Vec<SearchResult>, DomainError> {
        if results.is_empty() {
            return Ok(vec![]);
        }

        let query_hash = {
            let mut hasher = DefaultHasher::new();
            query.hash(&mut hasher);
            hasher.finish()
        };

        let mut reranked: Vec<SearchResult> = results
            .into_iter()
            .map(|result| {
                let mut hasher = DefaultHasher::new();
                query_hash.hash(&mut hasher);
                result.chunk().content().hash(&mut hasher);
                let score = (hasher.finish() % 10000) as f32 / 10000.0;
                SearchResult::new(result.chunk().clone(), score)
            })
            .collect();

        reranked.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(k) = top_k {
            reranked.truncate(k);
        }

        Ok(reranked)
    }

    fn model_name(&self) -> &str {
        "mock-reranking"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CodeChunk, Language, NodeType};

    #[tokio::test]
    async fn test_mock_reranking_consistency() {
        let service = MockReranking::new();

        let chunk = CodeChunk::new(
            "test.rs".to_string(),
            "fn test() {}".to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            "repo1".to_string(),
        );

        let results = vec![SearchResult::new(chunk, 0.5)];

        let reranked1 = service
            .rerank("test query", results.clone(), None)
            .await
            .unwrap();
        let reranked2 = service.rerank("test query", results, None).await.unwrap();

        assert_eq!(reranked1[0].score(), reranked2[0].score());
    }

    #[tokio::test]
    async fn test_mock_reranking_truncates() {
        let service = MockReranking::new();

        let chunks: Vec<SearchResult> = (0..10)
            .map(|i| {
                SearchResult::new(
                    CodeChunk::new(
                        format!("test{}.rs", i),
                        format!("fn test{}() {{}}", i),
                        1,
                        1,
                        Language::Rust,
                        NodeType::Function,
                        "repo1".to_string(),
                    ),
                    0.5,
                )
            })
            .collect();

        let reranked = service.rerank("query", chunks, Some(5)).await.unwrap();

        assert_eq!(reranked.len(), 5);
    }
}
