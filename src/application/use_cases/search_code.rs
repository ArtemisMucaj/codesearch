use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

/// Global minimum score threshold applied to all search results before they are
/// returned to the caller, regardless of the search path taken (semantic,
/// text-search, RRF, or reranked).  Results below this value are consistently
/// uninformative and only add noise to the output.
const MIN_RESULT_SCORE: f32 = 0.1;

use crate::application::use_cases::graph_expansion::GraphExpansionUseCase;
use crate::application::use_cases::rrf_fuse::rrf_fuse;
use crate::application::{EmbeddingService, QueryExpander, RerankingService, VectorRepository};
use crate::domain::{DomainError, SearchQuery, SearchResult};

pub struct SearchCodeUseCase {
    vector_repo: Arc<dyn VectorRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
    reranking_service: Option<Arc<dyn RerankingService>>,
    query_expander: Option<Arc<dyn QueryExpander>>,
    graph_expansion: Option<Arc<GraphExpansionUseCase>>,
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
            graph_expansion: None,
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

    /// Enable the call-graph expansion leg: top hits seed a walk over the
    /// call graph and structurally connected chunks are fused into the
    /// result list.
    pub fn with_graph_expansion(mut self, expansion: Arc<GraphExpansionUseCase>) -> Self {
        self.graph_expansion = Some(expansion);
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
            let base = if query.limit() <= 10 {
                20
            } else {
                query.limit()
            };
            let extra = ((base as f64) / (base as f64).ln()).ceil() as usize;
            base + extra
        } else {
            query.limit()
        };

        if fetch_limit != query.limit() {
            info!(
                "Using fetch_limit={} (target={}, +{} extra for reranking headroom)",
                fetch_limit,
                query.limit(),
                fetch_limit - query.limit()
            );
        }

        let mut search_query = if fetch_limit != query.limit() {
            query.clone().with_limit(fetch_limit)
        } else {
            query.clone()
        };

        // When the store holds no vectors (indexed with --no-embeddings),
        // skip query embedding entirely and force the keyword leg so the
        // BM25 + graph legs carry the search on their own.
        let semantic_available = match self.vector_repo.has_embeddings().await {
            Ok(available) => available,
            Err(e) => {
                warn!("Failed to probe for embeddings (assuming present): {}", e);
                true
            }
        };
        if !semantic_available {
            info!("No embeddings indexed; searching with keyword + graph legs only");
            search_query = search_query.with_text_search(true);
        }

        // The repository fuses two legs — BM25 and semantic — using RRF when
        // query.is_text_search() is true.
        let mut results = if let Some(expander) =
            self.query_expander.as_ref().filter(|_| semantic_available)
        {
            // --- Query expansion path ---
            // Expand the original query into multiple variants, embed each, search
            // for each independently, then fuse all result lists with RRF.
            let variants = expander.expand(query.query()).await?;
            info!("Query expanded into {} variants", variants.len());
            for (i, variant) in variants.iter().enumerate() {
                info!("  expanded query[{}]: {}", i, variant);
            }

            let mut set = tokio::task::JoinSet::new();
            for variant in variants {
                let embedding_service = self.embedding_service.clone();
                let vector_repo = self.vector_repo.clone();
                let search_query = search_query.clone();
                set.spawn(async move {
                    let embedding = embedding_service.embed_query(&variant).await?;
                    vector_repo.search(Some(&embedding), &search_query).await
                });
            }

            let mut all_results: Vec<Vec<SearchResult>> = Vec::with_capacity(set.len());
            while let Some(res) = set.join_next().await {
                all_results.push(res.map_err(|e| DomainError::StorageError(e.to_string()))??);
            }

            let total_pre_fusion: usize = all_results.iter().map(|r| r.len()).sum();
            let num_searches = all_results.len();
            let fused = rrf_fuse(all_results, fetch_limit);
            info!(
                "RRF fusion: {} candidates across {} variant searches -> {} fused results (capped at fetch_limit={})",
                total_pre_fusion,
                num_searches,
                fused.len(),
                fetch_limit,
            );
            fused
        } else {
            // --- Standard single-query path ---
            // `None` tells the repository to skip the semantic leg
            // (see VectorRepository::search).
            let query_embedding = if semantic_available {
                Some(self.embedding_service.embed_query(query.query()).await?)
            } else {
                None
            };
            self.vector_repo
                .search(query_embedding.as_deref(), &search_query)
                .await?
        };

        // Graph expansion leg: expand the top hits through the call graph and
        // fuse structurally connected chunks into the list.  Failures degrade
        // to the un-expanded results — the leg is additive, never required.
        let mut graph_fused = false;
        if let Some(ref expansion) = self.graph_expansion {
            match expansion.expand(&results, &query).await {
                Ok(graph_leg) if !graph_leg.is_empty() => {
                    let graph_len = graph_leg.len();
                    results = rrf_fuse(vec![results, graph_leg], fetch_limit);
                    graph_fused = true;
                    info!(
                        "Graph expansion: fused {} structurally related chunks -> {} results",
                        graph_len,
                        results.len()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("Graph expansion failed (continuing without): {}", e);
                }
            }
        }

        let mut reranked = false;
        if let Some(ref reranker) = self.reranking_service {
            // Filter out very low-scoring results before reranking — they are
            // unlikely to resurface and just slow down the cross-encoder.
            // Skip this filter for hybrid/RRF results: RRF scores are ~0.016–0.033
            // by design and would all be dropped by a hard >= 0.1 threshold.
            if !search_query.is_text_search() && self.query_expander.is_none() && !graph_fused {
                let before_filter = results.len();
                results.retain(|r| r.score() >= MIN_RESULT_SCORE);
                let filtered = before_filter - results.len();
                if filtered > 0 {
                    warn!(
                        "Excluded {} candidates with score < {:.2} before reranking",
                        filtered, MIN_RESULT_SCORE
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
            reranked = true;
        }

        // Drop anything that fell below the global quality floor.  Only applied
        // after cross-encoder reranking, where all scores are on a comparable
        // scale; raw RRF/hybrid scores are too low (~0.016–0.033) to use this
        // threshold meaningfully.
        if reranked {
            let before_global_filter = results.len();
            results.retain(|r| r.score() >= MIN_RESULT_SCORE);
            let global_filtered = before_global_filter - results.len();
            if global_filtered > 0 {
                warn!(
                    "Dropped {} low-value results with score < {:.2}",
                    global_filtered, MIN_RESULT_SCORE
                );
            }
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
