//! Persistence port for the experimental append-only claim graph.
//!
//! This is the storage contract for the memory redesign
//! (`docs/plans/2026-07-21-memory-claim-graph-design.md`). It lives alongside
//! the existing [`MemoryRepository`](super::MemoryRepository) rather than
//! replacing it: the claim log, typed edges, and resolved entities are new
//! side structures we can validate before rewiring the read/write paths.
//!
//! The claim layer is **append-only**. The only sanctioned destructive
//! operation is [`ClaimRepository::delete_claims_for_session`], used by a forced
//! re-import (see the design's §6); everything else appends or flips lifecycle
//! status.

use async_trait::async_trait;

use crate::domain::{Claim, ClaimEdge, ClaimStatus, DomainError, Entity};

/// Persistence port for claims, typed edges, and resolved entities.
///
/// Vectors are the embedding of the claim `statement` / entity name; `None`
/// when embeddings are unavailable (the row stays keyword-searchable).
#[async_trait]
pub trait ClaimRepository: Send + Sync {
    // ── Claims (append-only log) ─────────────────────────────────────────

    /// Append a claim to the log, with its optional statement embedding.
    ///
    /// The claim id is expected to be unique; appending a claim whose id
    /// already exists replaces that row (idempotent re-append), but callers on
    /// the ingestion path always mint a fresh id — an "update" is a new claim.
    async fn append_claim(&self, claim: &Claim, vector: Option<&[f32]>) -> Result<(), DomainError>;

    /// Fetch a claim by id.
    async fn find_claim(&self, id: &str) -> Result<Option<Claim>, DomainError>;

    /// List claims, optionally restricted to one `status` and/or `project`
    /// (global claims plus that project's), newest first.
    async fn list_claims(
        &self,
        status: Option<ClaimStatus>,
        project: Option<&str>,
    ) -> Result<Vec<Claim>, DomainError>;

    /// Transition a claim's lifecycle status, optionally closing its validity
    /// window (`valid_to`). Returns whether the claim existed. This is the
    /// non-destructive way an append-only store retires a claim (e.g. flipping
    /// it to `superseded` when a newer claim supersedes it).
    async fn set_claim_status(
        &self,
        id: &str,
        status: ClaimStatus,
        valid_to: Option<i64>,
    ) -> Result<bool, DomainError>;

    /// Hard-delete every claim whose provenance is `session_id`, along with its
    /// vector and any edges touching it. The single sanctioned destructive
    /// operation, used only by a forced re-import: re-running extraction over an
    /// unchanged transcript is a do-over, not a new observation, so the prior
    /// run's claims are wiped rather than tombstoned. Returns the number of
    /// claims removed.
    async fn delete_claims_for_session(&self, session_id: &str) -> Result<usize, DomainError>;

    // ── Typed edges ──────────────────────────────────────────────────────

    /// Insert (or replace) a typed edge between two claims, keyed by
    /// `(from, to, type)`.
    async fn add_edge(&self, edge: &ClaimEdge) -> Result<(), DomainError>;

    /// Edges originating at `claim_id`.
    async fn edges_from(&self, claim_id: &str) -> Result<Vec<ClaimEdge>, DomainError>;

    /// Edges pointing at `claim_id`.
    async fn edges_to(&self, claim_id: &str) -> Result<Vec<ClaimEdge>, DomainError>;

    // ── Entities ─────────────────────────────────────────────────────────

    /// Insert or replace an entity (and its aliases), keyed by id, with its
    /// optional name embedding.
    async fn upsert_entity(
        &self,
        entity: &Entity,
        vector: Option<&[f32]>,
    ) -> Result<(), DomainError>;

    /// Fetch an entity by id.
    async fn find_entity(&self, id: &str) -> Result<Option<Entity>, DomainError>;

    /// Resolve an entity by an exact (case-insensitive) match on its canonical
    /// name or any alias. The cheap first leg of entity resolution.
    async fn find_entity_by_alias(&self, alias: &str) -> Result<Option<Entity>, DomainError>;

    /// List all entities, newest first.
    async fn list_entities(&self) -> Result<Vec<Entity>, DomainError>;

    /// Cosine-similarity search over entity name embeddings — the fuzzy second
    /// leg of entity resolution. Returns `(entity, score)` best first.
    async fn search_entities_semantic(
        &self,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<(Entity, f32)>, DomainError>;

    // ── Claim retrieval (entry-point finder) ─────────────────────────────

    /// Cosine-similarity search over `active` claim statement embeddings,
    /// filtered to `project` (globals plus that project's). Returns
    /// `(claim, score)` best first, score in `[0, 1]`.
    async fn search_claims_semantic(
        &self,
        vector: &[f32],
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Claim, f32)>, DomainError>;

    /// Case-insensitive keyword search over `active` claim statements, filtered
    /// to `project`. Returns `(claim, score)` best first.
    async fn search_claims_keyword(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Claim, f32)>, DomainError>;

    /// Aggregate store statistics.
    async fn stats(&self) -> Result<crate::domain::ClaimStoreStats, DomainError>;
}
