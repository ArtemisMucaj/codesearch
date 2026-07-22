//! Claim ingestion — the online write path for the experimental claim graph.
//!
//! Flow (one bounded LLM call with a format-recovery retry):
//!
//! 1. **Idempotence / re-import** — a non-forced re-ingest of a session that
//!    already produced claims is skipped; a forced one hard-deletes the
//!    session's prior claims first (the sanctioned destructive op, design §6).
//! 2. **Prefetch** — semantic-search existing active claims for context, so the
//!    model can relate a new claim to a prior one instead of duplicating it.
//! 3. **Extract** — one call returns atomic subject–predicate–object claims,
//!    each with an optional typed relation to a prefetched claim.
//! 4. **Apply** — resolve entities (alias match, else create), append each claim
//!    with its statement embedding, and add any relation edge (flipping a
//!    superseded target to `superseded`).
//!
//! This is the append-only counterpart to
//! [`MemoryExtractionUseCase`](super::MemoryExtractionUseCase): it never
//! rewrites a claim in place.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::application::interfaces::{ChatClient, ClaimRepository, EmbeddingService};
use crate::application::use_cases::claim_ingestion_prompt as prompt;
use crate::application::use_cases::memory_extraction::{
    extract_json_object, repair_json_string_escapes,
};
use crate::application::use_cases::memory_support::unix_now;
use crate::domain::{
    Claim, ClaimEdge, ClaimStatus, DomainError, EdgeOrigin, EdgeType, Entity, EntityRef,
    SessionTranscript, SourceKind,
};

/// How many prior claims are prefetched into the extraction context.
const PREFETCH_LIMIT: usize = 8;

/// Upper bound on claims applied from a single ingestion, guarding against a
/// runaway model flooding the store.
const MAX_CLAIMS_PER_RUN: usize = 32;

/// What one ingestion produced.
#[derive(Debug, Default, PartialEq)]
pub struct IngestionReport {
    pub claims_written: usize,
    pub entities_created: usize,
    pub edges_added: usize,
}

/// Outcome of an ingestion request.
#[derive(Debug)]
pub enum IngestionOutcome {
    /// Extraction ran; the report describes what was written.
    Ingested(IngestionReport),
    /// The session already had claims and `force` was not set.
    AlreadyIngested,
}

/// JSON shape the extraction model must return (mirrors [`prompt::schema`]).
#[derive(Debug, Deserialize)]
struct RawIngestion {
    #[serde(default)]
    claims: Vec<RawClaim>,
}

#[derive(Debug, Deserialize)]
struct RawClaim {
    #[serde(default)]
    subject: String,
    #[serde(default)]
    subject_is_entity: bool,
    #[serde(default)]
    predicate: String,
    #[serde(default)]
    object: String,
    #[serde(default)]
    object_is_entity: bool,
    #[serde(default)]
    statement: String,
    #[serde(default)]
    source_kind: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    relation: Option<RawRelation>,
}

#[derive(Debug, Deserialize)]
struct RawRelation {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    target: String,
}

pub struct ClaimIngestionUseCase {
    chat_client: Arc<dyn ChatClient>,
    claim_repo: Arc<dyn ClaimRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl ClaimIngestionUseCase {
    pub fn new(
        chat_client: Arc<dyn ChatClient>,
        claim_repo: Arc<dyn ClaimRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            chat_client,
            claim_repo,
            embedding_service,
        }
    }

    /// Ingest `transcript` into the claim graph.
    #[tracing::instrument(skip_all, fields(session_id = %transcript.id))]
    pub async fn execute(
        &self,
        transcript: &SessionTranscript,
        force: bool,
    ) -> Result<IngestionOutcome, DomainError> {
        if force {
            let removed = self
                .claim_repo
                .delete_claims_for_session(&transcript.id)
                .await?;
            if removed > 0 {
                debug!(
                    "claim ingestion: forced re-import removed {removed} prior claims for '{}'",
                    transcript.id
                );
            }
        } else if self
            .claim_repo
            .count_claims_for_session(&transcript.id)
            .await?
            > 0
        {
            return Ok(IngestionOutcome::AlreadyIngested);
        }

        let prior = self.prefetch(transcript).await;
        let raw = self.extract(transcript, &prior).await?;
        let report = self.apply(transcript, raw, &prior).await?;
        Ok(IngestionOutcome::Ingested(report))
    }

    /// Semantic-search existing active claims for context. Failures degrade to
    /// "no context" rather than failing the ingest.
    async fn prefetch(&self, transcript: &SessionTranscript) -> Vec<Claim> {
        if !self.embedding_service.embeddings_enabled() {
            return Vec::new();
        }
        let query = prompt::prefetch_query(transcript);
        if query.trim().is_empty() {
            return Vec::new();
        }
        match self.embedding_service.embed_query(&query).await {
            Ok(vector) => match self
                .claim_repo
                .search_claims_semantic(&vector, transcript.project.as_deref(), PREFETCH_LIMIT)
                .await
            {
                Ok(results) => results.into_iter().map(|(claim, _)| claim).collect(),
                Err(e) => {
                    warn!("claim prefetch search failed: {e}");
                    Vec::new()
                }
            },
            Err(e) => {
                warn!("claim prefetch embedding failed: {e}");
                Vec::new()
            }
        }
    }

    /// Call the extraction model and parse its JSON, retrying once with a
    /// format-correction message when parsing fails.
    async fn extract(
        &self,
        transcript: &SessionTranscript,
        prior: &[Claim],
    ) -> Result<RawIngestion, DomainError> {
        let system = prompt::system_prompt();
        let user = prompt::user_prompt(transcript, prior);
        let schema = prompt::schema();
        let response = self
            .chat_client
            .complete_json(&system, &user, "claim_ingestion", &schema)
            .await?;
        match parse_ingestion(&response) {
            Ok(parsed) => Ok(parsed),
            Err(first_err) => {
                debug!("claim ingestion output unparseable, retrying once: {first_err}");
                let retry_user = format!("{user}\n\n{}", prompt::format_retry_prompt());
                let response = self
                    .chat_client
                    .complete_json(&system, &retry_user, "claim_ingestion", &schema)
                    .await?;
                parse_ingestion(&response).map_err(|e| {
                    DomainError::parse(format!(
                        "claim ingestion model returned unparseable output twice: {e}"
                    ))
                })
            }
        }
    }

    /// Resolve entities, append claims, and add relation edges.
    async fn apply(
        &self,
        transcript: &SessionTranscript,
        raw: RawIngestion,
        prior: &[Claim],
    ) -> Result<IngestionReport, DomainError> {
        let now = unix_now();
        let prior_ids: HashSet<&str> = prior.iter().map(|c| c.id.as_str()).collect();
        let mut entity_cache: HashMap<String, String> = HashMap::new();
        let mut report = IngestionReport::default();

        for raw_claim in raw.claims {
            if report.claims_written >= MAX_CLAIMS_PER_RUN {
                break;
            }
            let statement = raw_claim.statement.trim();
            if statement.is_empty() || raw_claim.predicate.trim().is_empty() {
                continue;
            }

            let subject = self
                .resolve_ref(
                    &raw_claim.subject,
                    raw_claim.subject_is_entity,
                    now,
                    &mut entity_cache,
                    &mut report.entities_created,
                )
                .await?;
            let object = self
                .resolve_ref(
                    &raw_claim.object,
                    raw_claim.object_is_entity,
                    now,
                    &mut entity_cache,
                    &mut report.entities_created,
                )
                .await?;

            let source_kind =
                SourceKind::parse(&raw_claim.source_kind).unwrap_or(SourceKind::AssistantInferred);
            let claim = Claim {
                id: Uuid::new_v4().to_string(),
                subject,
                predicate: raw_claim.predicate.trim().to_string(),
                object,
                statement: statement.to_string(),
                project: transcript.project.clone(),
                recorded_at: now,
                valid_from: now,
                valid_to: None,
                source_session_id: Some(transcript.id.clone()),
                source_message_index: None,
                source_kind,
                confidence: raw_claim.confidence.clamp(0.0, 1.0),
                status: ClaimStatus::Active,
                derived: false,
                derived_from: Vec::new(),
            };
            let vector = self.embed_opt(&claim.statement).await;
            self.claim_repo
                .append_claim(&claim, vector.as_deref())
                .await?;
            report.claims_written += 1;

            // A relation is only honored when it points at a prefetched claim
            // (guards against the model inventing a target id) and names a
            // known edge type.
            if let Some(rel) = raw_claim.relation {
                let target = rel.target.trim();
                let Some(edge_type) = EdgeType::parse(&rel.kind) else {
                    continue;
                };
                if target.is_empty() || !prior_ids.contains(target) {
                    continue;
                }
                self.claim_repo
                    .add_edge(&ClaimEdge {
                        from_claim: claim.id.clone(),
                        to_claim: target.to_string(),
                        edge_type,
                        created_at: now,
                        created_by: EdgeOrigin::Ingestion,
                        confidence: claim.confidence,
                    })
                    .await?;
                report.edges_added += 1;
                // Supersession retires the prior claim non-destructively.
                if edge_type == EdgeType::Supersedes {
                    self.claim_repo
                        .set_claim_status(target, ClaimStatus::Superseded, Some(now))
                        .await?;
                }
            }
        }
        Ok(report)
    }

    /// Resolve a subject/object surface form to an [`EntityRef`]. Literals pass
    /// through; entity references resolve against existing aliases and are
    /// created (and embedded) on first sight, cached within the run.
    async fn resolve_ref(
        &self,
        surface: &str,
        is_entity: bool,
        now: i64,
        cache: &mut HashMap<String, String>,
        created: &mut usize,
    ) -> Result<EntityRef, DomainError> {
        let trimmed = surface.trim();
        if !is_entity || trimmed.is_empty() {
            return Ok(EntityRef::Literal(trimmed.to_string()));
        }
        let key = trimmed.to_lowercase();
        if let Some(id) = cache.get(&key) {
            return Ok(EntityRef::Entity(id.clone()));
        }
        if let Some(existing) = self.claim_repo.find_entity_by_alias(trimmed).await? {
            cache.insert(key, existing.id.clone());
            return Ok(EntityRef::Entity(existing.id));
        }
        let entity = Entity {
            id: Uuid::new_v4().to_string(),
            entity_type: "unknown".to_string(),
            canonical_name: trimmed.to_string(),
            aliases: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let vector = self.embed_opt(&entity.canonical_name).await;
        self.claim_repo
            .upsert_entity(&entity, vector.as_deref())
            .await?;
        *created += 1;
        cache.insert(key, entity.id.clone());
        Ok(EntityRef::Entity(entity.id))
    }

    /// Embed `text`, returning `None` when embeddings are disabled or the call
    /// fails (the row stays keyword-searchable either way).
    async fn embed_opt(&self, text: &str) -> Option<Vec<f32>> {
        if !self.embedding_service.embeddings_enabled() {
            return None;
        }
        match self.embedding_service.embed_query(text).await {
            Ok(vector) => Some(vector),
            Err(e) => {
                warn!("failed to embed claim text: {e}");
                None
            }
        }
    }
}

/// Parse the model's ingestion JSON, tolerating prose/fences and the
/// invalid-escape output small local models emit.
fn parse_ingestion(response: &str) -> Result<RawIngestion, DomainError> {
    let json = extract_json_object(response)
        .ok_or_else(|| DomainError::parse("no JSON object found in ingestion output"))?;
    match serde_json::from_str::<RawIngestion>(json) {
        Ok(parsed) => Ok(parsed),
        Err(strict_err) => {
            let repaired = repair_json_string_escapes(json);
            serde_json::from_str::<RawIngestion>(&repaired)
                .map_err(|_| DomainError::parse(format!("invalid ingestion JSON: {strict_err}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claims_and_relation() {
        let response = r#"{"claims": [
            {"subject": "Alice", "subject_is_entity": true, "predicate": "lives_in",
             "object": "Munich", "object_is_entity": true,
             "statement": "Alice lives in Munich", "source_kind": "user_stated",
             "confidence": 0.9,
             "relation": {"type": "supersedes", "target": "old-1"}}
        ]}"#;
        let parsed = parse_ingestion(response).unwrap();
        assert_eq!(parsed.claims.len(), 1);
        let c = &parsed.claims[0];
        assert_eq!(c.subject, "Alice");
        assert!(c.object_is_entity);
        let rel = c.relation.as_ref().unwrap();
        assert_eq!(rel.kind, "supersedes");
        assert_eq!(rel.target, "old-1");
    }

    #[test]
    fn parses_fenced_json_without_relation() {
        let response = "```json\n{\"claims\": [\
            {\"subject\": \"user\", \"subject_is_entity\": true, \"predicate\": \"prefers\", \
             \"object\": \"tabs\", \"object_is_entity\": false, \
             \"statement\": \"prefers tabs\", \"source_kind\": \"user_stated\", \
             \"confidence\": 0.8}]}\n```";
        let parsed = parse_ingestion(response).unwrap();
        assert_eq!(parsed.claims.len(), 1);
        assert!(parsed.claims[0].relation.is_none());
        assert!(!parsed.claims[0].object_is_entity);
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(parse_ingestion("I could not extract anything").is_err());
    }

    #[test]
    fn missing_fields_default_and_empty_claims_parse() {
        let parsed = parse_ingestion(r#"{"claims": []}"#).unwrap();
        assert!(parsed.claims.is_empty());
    }
}
