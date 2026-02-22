use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use crate::application::{EmbeddingService, QueryExpander, RerankingService, VectorRepository};
use crate::application::use_cases::rrf_fuse::rrf_fuse_many;
use crate::domain::{DomainError, SearchQuery, SearchResult};

pub struct SearchCodeUseCase {
    vector_repo: Arc<dyn VectorRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
    query_expander: Option<Arc<dyn QueryExpander>>,
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
            query_expander: None,
        }
    }

    pub fn with_reranking(mut self, service: Arc<dyn RerankingService>) -> Self {
        self.reranking_service = Some(service);
        self
    }

    pub fn with_query_expansion(mut self, expander: Arc<dyn QueryExpander>) -> Self {
        self.query_expander = Some(expander);
        self
    }

    pub async fn execute(&self, query: SearchQuery) -> Result<Vec<SearchResult>, DomainError> {
        info!(
            "Searching for: {} (text_search={}, expand_query={})",
            query.query(),
            query.is_text_search(),
            self.query_expander.is_some(),
        );

        let start_time = Instant::now();

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

        // The repository fuses two legs — BM25 and semantic — using RRF when
        // query.is_text_search() is true.
        let mut results = if let Some(ref expander) = self.query_expander {
            // --- Query expansion path ---
            // Expand the original query into multiple variants, embed each, search
            // for each independently, then fuse all result lists with RRF.
            let variants = expander.expand(query.query()).await?;
            info!("Query expanded into {} variants", variants.len());

            let mut all_results: Vec<Vec<SearchResult>> = Vec::with_capacity(variants.len());
            for variant in &variants {
                let embedding = self.embedding_service.embed_query(variant).await?;
                let variant_results = self.vector_repo.search(&embedding, &search_query).await?;
                all_results.push(variant_results);
            }

            rrf_fuse_many(all_results, fetch_limit)
        } else {
            // --- Standard single-query path ---
            let query_embedding = self.embedding_service.embed_query(query.query()).await?;
            self.vector_repo
                .search(&query_embedding, &search_query)
                .await?
        };

        if let Some(ref reranker) = self.reranking_service {
            // Filter out very low-scoring results before reranking — they are
            // unlikely to resurface and just slow down the cross-encoder.
            // Skip this filter for hybrid/RRF results: RRF scores are ~0.016–0.033
            // by design and would all be dropped by a hard >= 0.1 threshold.
            if !search_query.is_text_search() && self.query_expander.is_none() {
                let before_filter = results.len();
                results.retain(|r| r.score() >= 0.1);
                let filtered = before_filter - results.len();
                if filtered > 0 {
                    warn!(
                        "Excluded {} candidates with score < 0.1 before reranking",
                        filtered
                    );
                }
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
