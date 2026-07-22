//! The append-only claim graph model (experimental).
//!
//! This is the storage-facing vocabulary for the memory redesign described in
//! `docs/plans/2026-07-21-memory-claim-graph-design.md`: an immutable log of
//! [`Claim`]s linked by typed [`ClaimEdge`]s over resolved [`Entity`] nodes.
//! Unlike the current [`MemoryItem`](super::MemoryItem) store, a claim is never
//! rewritten in place — an "update" is a new claim plus a `supersedes` edge, and
//! the current-truth view is the set of `active` claims.
//!
//! The model deliberately keeps a **single event timeline** (`recorded_at`, with
//! `valid_to` closed on supersession) rather than an independent world-valid
//! time, and a coarse `(session_id, message_index)` provenance — the two
//! simplifications the codebase de-risking pass forced (see the design's
//! revision note).

use serde::{Deserialize, Serialize};

/// The subject or object of a [`Claim`]: either a resolved canonical entity
/// (referenced by id) or a literal value rendered as text.
///
/// A claim's subject is normally an [`EntityRef::Entity`]; its object may be
/// either (e.g. `has_pet -> Entity("dog_rex")` vs `prefers -> Literal("tabs")`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum EntityRef {
    /// Reference to an [`Entity`] by its id.
    Entity(String),
    /// A literal value (string, number, date — stored as text).
    Literal(String),
}

impl EntityRef {
    /// The referenced entity id, if this is an [`EntityRef::Entity`].
    pub fn entity_id(&self) -> Option<&str> {
        match self {
            EntityRef::Entity(id) => Some(id),
            EntityRef::Literal(_) => None,
        }
    }

    /// The literal value, if this is an [`EntityRef::Literal`].
    pub fn literal(&self) -> Option<&str> {
        match self {
            EntityRef::Literal(v) => Some(v),
            EntityRef::Entity(_) => None,
        }
    }

    /// Reconstruct from the `(entity_id, literal)` column pair used in storage.
    /// Prefers the entity id when both are somehow present.
    pub fn from_columns(entity_id: Option<String>, literal: Option<String>) -> Self {
        match entity_id {
            Some(id) => EntityRef::Entity(id),
            None => EntityRef::Literal(literal.unwrap_or_default()),
        }
    }
}

/// Where a claim came from, used to arbitrate contradictions. The ordering
/// `user_stated > assistant_inferred > derived` is the primary arbiter (see the
/// design's §7); `confidence` is only a tiebreak within one source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The user asserted it directly — the most trusted.
    UserStated,
    /// The assistant inferred it from context.
    AssistantInferred,
    /// Produced by the consolidation pass, not primary observation.
    Derived,
}

impl SourceKind {
    /// Stable identifier used in storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::UserStated => "user_stated",
            SourceKind::AssistantInferred => "assistant_inferred",
            SourceKind::Derived => "derived",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user_stated" => Some(SourceKind::UserStated),
            "assistant_inferred" => Some(SourceKind::AssistantInferred),
            "derived" => Some(SourceKind::Derived),
            _ => None,
        }
    }

    /// Trust rank; higher wins a contradiction. Used by arbitration.
    pub fn trust_rank(&self) -> u8 {
        match self {
            SourceKind::UserStated => 2,
            SourceKind::AssistantInferred => 1,
            SourceKind::Derived => 0,
        }
    }
}

/// Lifecycle state of a claim. The current-truth projection is the set of
/// [`ClaimStatus::Active`] claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// Current and believed true.
    Active,
    /// Replaced by a newer claim via a `supersedes` edge; kept for history.
    Superseded,
    /// Marked as never having been true (a bad extraction).
    Retracted,
    /// A conflict too ambiguous to settle inline; awaits consolidation.
    NeedsResolution,
}

impl ClaimStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaimStatus::Active => "active",
            ClaimStatus::Superseded => "superseded",
            ClaimStatus::Retracted => "retracted",
            ClaimStatus::NeedsResolution => "needs_resolution",
        }
    }

    pub fn parse(s: &str) -> Option<ClaimStatus> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Some(ClaimStatus::Active),
            "superseded" => Some(ClaimStatus::Superseded),
            "retracted" => Some(ClaimStatus::Retracted),
            "needs_resolution" => Some(ClaimStatus::NeedsResolution),
            _ => None,
        }
    }
}

/// The typed relationship between two claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    /// Temporal replacement — old was true, new is true now.
    Supersedes,
    /// Genuine conflict, no temporal ordering.
    Contradicts,
    /// Enrichment / specialization of a broader claim.
    Refines,
    /// The target claim was never true (bad extraction).
    Retracts,
    /// An independent source confirms the target claim.
    Corroborates,
    /// Generic association discovered later; navigational only.
    RelatesTo,
}

impl EdgeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeType::Supersedes => "supersedes",
            EdgeType::Contradicts => "contradicts",
            EdgeType::Refines => "refines",
            EdgeType::Retracts => "retracts",
            EdgeType::Corroborates => "corroborates",
            EdgeType::RelatesTo => "relates_to",
        }
    }

    pub fn parse(s: &str) -> Option<EdgeType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "supersedes" => Some(EdgeType::Supersedes),
            "contradicts" => Some(EdgeType::Contradicts),
            "refines" => Some(EdgeType::Refines),
            "retracts" => Some(EdgeType::Retracts),
            "corroborates" => Some(EdgeType::Corroborates),
            "relates_to" => Some(EdgeType::RelatesTo),
            _ => None,
        }
    }
}

/// Who created an edge — provenance for the graph itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOrigin {
    /// Written on the online ingestion path.
    Ingestion,
    /// Written by the offline consolidation ("dream") pass.
    Consolidation,
    /// Written by an explicit user/manual action.
    Manual,
}

impl EdgeOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeOrigin::Ingestion => "ingestion",
            EdgeOrigin::Consolidation => "consolidation",
            EdgeOrigin::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<EdgeOrigin> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ingestion" => Some(EdgeOrigin::Ingestion),
            "consolidation" => Some(EdgeOrigin::Consolidation),
            "manual" => Some(EdgeOrigin::Manual),
            _ => None,
        }
    }
}

/// A resolved, canonical entity: the anchor a claim's subject/object points at.
///
/// `aliases` are the surface forms that resolve to this entity (e.g. `"Alice"`,
/// `"my coworker Alice"`), kept separate from `canonical_name`. Entities are
/// global (not project-scoped); the project scope lives on the [`Claim`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    /// Coarse type: `person`, `place`, `project`, `tool`, …
    pub entity_type: String,
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A single immutable claim in the append-only log.
///
/// Fields are public because a claim is a record-like value (as with
/// [`ImportedSession`](super::ImportedSession) / [`DreamRun`](super::DreamRun)),
/// constructed by the ingestion path and read back by retrieval; it carries no
/// invariants beyond those the store enforces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    /// Resolved subject — normally an [`EntityRef::Entity`].
    pub subject: EntityRef,
    /// Predicate from a small controlled-ish vocabulary (e.g. `prefers`,
    /// `lives_in`, `uses`).
    pub predicate: String,
    pub object: EntityRef,
    /// Human-readable rendering of the triple, used for embedding and display.
    pub statement: String,
    /// Project/namespace scope, or `None` for a global claim. Resolved at
    /// ingestion by the shared repo resolver, mirroring `MemoryItem::project`.
    pub project: Option<String>,

    /// When this claim entered the log (the single event timeline).
    pub recorded_at: i64,
    /// Defaults to `recorded_at`; only distinct when an explicit date was lifted
    /// from the source text.
    pub valid_from: i64,
    /// Set to the recording time of the claim that supersedes this one; `None`
    /// while the claim is still current.
    pub valid_to: Option<i64>,

    /// Session this claim was extracted from (provenance, half 1).
    pub source_session_id: Option<String>,
    /// Index of the transcript message it came from (provenance, half 2).
    pub source_message_index: Option<i64>,
    pub source_kind: SourceKind,
    /// Best-effort confidence in `[0, 1]`; advisory (see §7).
    pub confidence: f32,

    pub status: ClaimStatus,
    /// True when produced by consolidation rather than ingestion.
    pub derived: bool,
    /// Source claim ids this was derived from (empty for primary claims).
    pub derived_from: Vec<String>,
}

/// A typed, directed edge between two claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimEdge {
    pub from_claim: String,
    pub to_claim: String,
    pub edge_type: EdgeType,
    pub created_at: i64,
    pub created_by: EdgeOrigin,
    pub confidence: f32,
}

/// Aggregate statistics about the claim store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimStoreStats {
    pub total_claims: u64,
    /// Claim counts by `status` string.
    pub claims_by_status: Vec<(String, u64)>,
    pub total_entities: u64,
    pub total_edges: u64,
}
