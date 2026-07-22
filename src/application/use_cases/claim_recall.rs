//! Claim recall — the read path over the experimental claim graph (design §10).
//!
//! Graph-anchored, not vector-first: a hybrid (semantic + keyword) search finds
//! *entry-point* claims, then a bounded 1-hop walk over enrichment edges
//! (`refines` / `corroborates` / `relates_to`) pulls in neighbors. Only `active`
//! claims surface — `supersedes` / `contradicts` edges are followed to
//! understand status, never to surface the stale claim itself. Results are
//! served from the active-claim view, so no conflict resolution happens here.
//!
//! Retrieval *quality* over this store is spike-gated (claim-embedding recall,
//! entity-resolution quality); this use case is the mechanism, to be measured on
//! real data before it replaces the shipped memory search.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::interfaces::{ClaimRepository, EmbeddingService};
use crate::domain::{Claim, ClaimStatus, DomainError, EdgeType};

/// RRF dampening constant (standard value used across the codebase).
const RRF_K: f32 = 60.0;

/// How many candidates each hybrid leg retrieves before fusion.
const CANDIDATES_PER_LEG: usize = 20;

/// How many top anchors are expanded across enrichment edges.
const MAX_EXPANSION_SEEDS: usize = 5;

/// Score multiplier applied to a claim pulled in only by graph expansion, so
/// enrichment neighbors rank below the anchors that found them.
const EXPANSION_DECAY: f32 = 0.3;

/// Enrichment edge types walked during expansion. `supersedes` / `contradicts`
/// / `retracts` are deliberately excluded — they explain status, they are not
/// navigational.
const EXPANSION_EDGES: [EdgeType; 3] = [
    EdgeType::Refines,
    EdgeType::Corroborates,
    EdgeType::RelatesTo,
];

pub struct ClaimRecallUseCase {
    claim_repo: Arc<dyn ClaimRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl ClaimRecallUseCase {
    pub fn new(
        claim_repo: Arc<dyn ClaimRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            claim_repo,
            embedding_service,
        }
    }

    /// Recall claims for `query`, scoped to `project` (globals plus that
    /// project's). Returns `(claim, score)` best first, active claims only.
    pub async fn execute(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Claim, f32)>, DomainError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(DomainError::invalid_input("query must not be empty"));
        }

        // ── Entry-point resolution: hybrid semantic + keyword, RRF-fused ──
        let semantic = if self.embedding_service.embeddings_enabled() {
            let vector = self.embedding_service.embed_query(query).await?;
            self.claim_repo
                .search_claims_semantic(&vector, project, CANDIDATES_PER_LEG)
                .await?
        } else {
            Vec::new()
        };
        let keyword = self
            .claim_repo
            .search_claims_keyword(query, project, CANDIDATES_PER_LEG)
            .await?;

        let mut fused: HashMap<String, (Claim, f32)> = HashMap::new();
        for leg in [semantic, keyword] {
            for (rank, (claim, _)) in leg.into_iter().enumerate() {
                let contribution = 1.0 / (RRF_K + rank as f32 + 1.0);
                fused
                    .entry(claim.id.clone())
                    .and_modify(|(_, score)| *score += contribution)
                    .or_insert((claim, contribution));
            }
        }

        // ── Graph expansion: 1 hop over enrichment edges from top anchors ──
        let mut seeds: Vec<(String, f32)> = fused
            .iter()
            .map(|(id, (_, score))| (id.clone(), *score))
            .collect();
        seeds.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        seeds.truncate(MAX_EXPANSION_SEEDS);

        for (seed_id, seed_score) in seeds {
            for neighbor_id in self.enrichment_neighbors(&seed_id).await? {
                if fused.contains_key(&neighbor_id) {
                    continue;
                }
                match self.claim_repo.find_claim(&neighbor_id).await? {
                    // Only surface neighbors that are themselves current.
                    Some(claim) if claim.status == ClaimStatus::Active => {
                        fused.insert(neighbor_id, (claim, seed_score * EXPANSION_DECAY));
                    }
                    _ => {}
                }
            }
        }

        let mut results: Vec<(Claim, f32)> = fused.into_values().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Ids of claims linked to `claim_id` by an enrichment edge, in either
    /// direction (a refines child and its parent are both relevant).
    async fn enrichment_neighbors(&self, claim_id: &str) -> Result<Vec<String>, DomainError> {
        let mut ids = Vec::new();
        let from = self.claim_repo.edges_from(claim_id).await?;
        let to = self.claim_repo.edges_to(claim_id).await?;
        for edge in from {
            if EXPANSION_EDGES.contains(&edge.edge_type) {
                ids.push(edge.to_claim);
            }
        }
        for edge in to {
            if EXPANSION_EDGES.contains(&edge.edge_type) {
                ids.push(edge.from_claim);
            }
        }
        Ok(ids)
    }
}
