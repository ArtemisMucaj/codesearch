//! Claim-graph consolidation — the offline "dream" pass (design §8).
//!
//! Clusters near-duplicate **primary** claims by embedding similarity and asks a
//! stronger model to abstract each cluster into higher-level *derived* claims
//! (episodic → semantic). The guardrails are inverted relative to the shipped
//! memory dream: the immutable claim layer is sacrosanct here, so this pass only
//! ever **adds** derived claims and `refines` edges — it never rewrites, retires,
//! or deletes a primary claim.
//!
//! Convergence: a cluster whose members are already covered by an existing
//! derived claim (via `derived_from`) is skipped, so repeated passes over stable
//! input do no work.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::application::interfaces::{ChatClient, ClaimRepository, EmbeddingService};
use crate::application::use_cases::claim_dream_prompt as prompt;
use crate::application::use_cases::memory_extraction::{
    extract_json_object, repair_json_string_escapes,
};
use crate::application::use_cases::memory_support::unix_now;
use crate::domain::{
    cosine_similarity, Claim, ClaimEdge, ClaimStatus, DomainError, EdgeOrigin, EdgeType, EntityRef,
    SourceKind,
};

/// Cosine similarity above which two claims are clustered for consolidation.
const SIMILARITY_THRESHOLD: f32 = 0.82;

/// Most clusters examined per cycle (largest first); the rest wait for the next.
const MAX_CLUSTERS_PER_RUN: usize = 8;

/// Most claims sent to the model per cluster.
const MAX_CLUSTER_CLAIMS: usize = 8;

/// Upper bound on derived claims added by one cycle.
const MAX_DERIVED_PER_RUN: usize = 8;

/// Derived claims are always less trusted than primary observations.
const DERIVED_CONFIDENCE_CAP: f32 = 0.8;

/// What one consolidation cycle did.
#[derive(Debug, Default, PartialEq)]
pub struct ClaimDreamReport {
    /// Similarity clusters of primary claims found.
    pub clusters_examined: usize,
    /// Clusters skipped because they were already abstracted (convergence).
    pub clusters_skipped_stable: usize,
    /// Derived claims appended.
    pub derived_claims_added: usize,
    /// `refines` edges added (source → derived).
    pub edges_added: usize,
}

/// JSON shape the abstraction model must return (mirrors [`prompt::schema`]).
#[derive(Debug, Deserialize)]
struct RawDream {
    #[serde(default)]
    derived: Vec<RawDerived>,
}

#[derive(Debug, Deserialize)]
struct RawDerived {
    #[serde(default)]
    statement: String,
    #[serde(default)]
    confidence: f32,
}

pub struct ClaimDreamUseCase {
    claim_repo: Arc<dyn ClaimRepository>,
    chat_client: Arc<dyn ChatClient>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl ClaimDreamUseCase {
    pub fn new(
        claim_repo: Arc<dyn ClaimRepository>,
        chat_client: Arc<dyn ChatClient>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            claim_repo,
            chat_client,
            embedding_service,
        }
    }

    /// Run one consolidation cycle.
    #[tracing::instrument(skip_all)]
    pub async fn execute(&self) -> Result<ClaimDreamReport, DomainError> {
        let mut report = ClaimDreamReport::default();

        let active = self
            .claim_repo
            .list_claims(Some(ClaimStatus::Active), None)
            .await?;
        // Never abstract abstractions: cluster only primary claims.
        let primaries: Vec<Claim> = active.iter().filter(|c| !c.derived).cloned().collect();

        // Source ids already covered by an existing derived claim — the basis of
        // convergence.
        let covered: HashSet<String> = active
            .iter()
            .filter(|c| c.derived)
            .flat_map(|c| c.derived_from.iter().cloned())
            .collect();

        let clusters = self.build_clusters(&primaries).await?;
        report.clusters_examined = clusters.len();

        let mut derived_budget = MAX_DERIVED_PER_RUN;
        for cluster in clusters {
            if derived_budget == 0 {
                break;
            }
            // Convergence: skip a cluster already fully covered by a derived claim.
            if cluster.iter().all(|c| covered.contains(&c.id)) {
                report.clusters_skipped_stable += 1;
                continue;
            }
            let derived = match self.abstract_cluster(&cluster).await {
                Ok(d) => d,
                Err(e) => {
                    warn!("claim dream abstraction call failed, skipping cluster: {e}");
                    continue;
                }
            };
            for raw in derived {
                if derived_budget == 0 {
                    break;
                }
                let statement = raw.statement.trim();
                if statement.is_empty() {
                    continue;
                }
                self.append_derived(statement, raw.confidence, &cluster, &mut report)
                    .await?;
                derived_budget -= 1;
            }
        }

        info!(
            "claim dream: {} clusters, {} skipped, {} derived, {} edges",
            report.clusters_examined,
            report.clusters_skipped_stable,
            report.derived_claims_added,
            report.edges_added
        );
        Ok(report)
    }

    /// Append one derived claim over `cluster` plus its `refines` edges. Never
    /// touches the source claims.
    async fn append_derived(
        &self,
        statement: &str,
        model_confidence: f32,
        cluster: &[Claim],
        report: &mut ClaimDreamReport,
    ) -> Result<(), DomainError> {
        let now = unix_now();
        // Derived confidence stays at or below both the model's own estimate and
        // the weakest specific, and never exceeds the cap.
        let min_source = cluster
            .iter()
            .map(|c| c.confidence)
            .fold(f32::INFINITY, f32::min);
        let confidence = model_confidence
            .clamp(0.0, 1.0)
            .min(min_source)
            .min(DERIVED_CONFIDENCE_CAP);

        // A cluster of near-duplicates shares a subject and (usually) a project.
        let subject = cluster
            .first()
            .map(|c| c.subject.clone())
            .unwrap_or_else(|| EntityRef::Literal(String::new()));
        let project = shared_project(cluster);

        let derived = Claim {
            id: Uuid::new_v4().to_string(),
            subject,
            predicate: "generalizes".to_string(),
            object: EntityRef::Literal(String::new()),
            statement: statement.to_string(),
            project,
            recorded_at: now,
            valid_from: now,
            valid_to: None,
            source_session_id: None,
            source_message_index: None,
            source_kind: SourceKind::Derived,
            confidence,
            status: ClaimStatus::Active,
            derived: true,
            derived_from: cluster.iter().map(|c| c.id.clone()).collect(),
        };
        let vector = self.embed_opt(&derived.statement).await;
        self.claim_repo
            .append_claim(&derived, vector.as_deref())
            .await?;
        report.derived_claims_added += 1;

        // Each specific refines the abstraction (both stay true).
        for source in cluster {
            self.claim_repo
                .add_edge(&ClaimEdge {
                    from_claim: source.id.clone(),
                    to_claim: derived.id.clone(),
                    edge_type: EdgeType::Refines,
                    created_at: now,
                    created_by: EdgeOrigin::Consolidation,
                    confidence,
                })
                .await?;
            report.edges_added += 1;
        }
        Ok(())
    }

    /// Group primary claims into similarity clusters (connected components over
    /// pairs whose embedding cosine crosses the threshold). Claims without a
    /// stored vector are left alone.
    async fn build_clusters(&self, primaries: &[Claim]) -> Result<Vec<Vec<Claim>>, DomainError> {
        let vectors = self.claim_repo.list_claim_vectors().await?;
        let by_id: HashMap<&str, &Vec<f32>> =
            vectors.iter().map(|(id, v)| (id.as_str(), v)).collect();
        let embedded: Vec<(&Claim, &Vec<f32>)> = primaries
            .iter()
            .filter_map(|c| by_id.get(c.id.as_str()).map(|v| (c, *v)))
            .collect();

        let mut parent: Vec<usize> = (0..embedded.len()).collect();
        for a in 0..embedded.len() {
            for b in (a + 1)..embedded.len() {
                if cosine_similarity(embedded[a].1, embedded[b].1) >= SIMILARITY_THRESHOLD {
                    union(&mut parent, a, b);
                }
            }
        }

        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for idx in 0..embedded.len() {
            groups.entry(find(&mut parent, idx)).or_default().push(idx);
        }
        let mut clusters: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() >= 2).collect();
        for cluster in &mut clusters {
            // Deterministic order; cap the number sent to the model.
            cluster.sort_by(|&a, &b| embedded[a].0.id.cmp(&embedded[b].0.id));
            cluster.truncate(MAX_CLUSTER_CLAIMS);
        }
        // Largest (most redundant) clusters first, with a stable tiebreak.
        clusters.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then_with(|| embedded[a[0]].0.id.cmp(&embedded[b[0]].0.id))
        });
        clusters.truncate(MAX_CLUSTERS_PER_RUN);
        debug!("claim dream: {} consolidation clusters", clusters.len());
        Ok(clusters
            .into_iter()
            .map(|g| g.into_iter().map(|idx| embedded[idx].0.clone()).collect())
            .collect())
    }

    /// One abstraction call for one cluster, with a format-recovery retry.
    async fn abstract_cluster(&self, cluster: &[Claim]) -> Result<Vec<RawDerived>, DomainError> {
        let system = prompt::system_prompt();
        let user = prompt::user_prompt(cluster);
        let schema = prompt::schema();
        let response = self
            .chat_client
            .complete_json(&system, &user, "claim_dream", &schema)
            .await?;
        match parse_dream(&response) {
            Ok(parsed) => Ok(parsed.derived),
            Err(first_err) => {
                debug!("claim dream output unparseable, retrying once: {first_err}");
                let retry_user = format!("{user}\n\n{}", prompt::format_retry_prompt());
                let response = self
                    .chat_client
                    .complete_json(&system, &retry_user, "claim_dream", &schema)
                    .await?;
                parse_dream(&response)
                    .map(|p| p.derived)
                    .map_err(|e| DomainError::parse(format!("dream output unparseable twice: {e}")))
            }
        }
    }

    /// Embed `text`, returning `None` when embeddings are disabled or the call
    /// fails (the derived claim stays keyword-searchable either way).
    async fn embed_opt(&self, text: &str) -> Option<Vec<f32>> {
        if !self.embedding_service.embeddings_enabled() {
            return None;
        }
        match self.embedding_service.embed_query(text).await {
            Ok(vector) => Some(vector),
            Err(e) => {
                warn!("failed to embed derived claim: {e}");
                None
            }
        }
    }
}

/// The project shared by every claim in `cluster`, or `None` if they differ.
fn shared_project(cluster: &[Claim]) -> Option<String> {
    let mut iter = cluster.iter().map(|c| c.project.as_deref());
    let first = iter.next().flatten();
    let first = first?;
    if cluster.iter().all(|c| c.project.as_deref() == Some(first)) {
        Some(first.to_string())
    } else {
        None
    }
}

fn parse_dream(response: &str) -> Result<RawDream, DomainError> {
    let json = extract_json_object(response)
        .ok_or_else(|| DomainError::parse("no JSON object found in dream output"))?;
    match serde_json::from_str::<RawDream>(json) {
        Ok(parsed) => Ok(parsed),
        Err(strict_err) => {
            let repaired = repair_json_string_escapes(json);
            serde_json::from_str::<RawDream>(&repaired)
                .map_err(|_| DomainError::parse(format!("invalid dream JSON: {strict_err}")))
        }
    }
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb] = ra;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_derived_claims() {
        let response = r#"{"derived": [
            {"statement": "tends to work late around releases", "confidence": 0.7}
        ]}"#;
        let parsed = parse_dream(response).unwrap();
        assert_eq!(parsed.derived.len(), 1);
        assert_eq!(
            parsed.derived[0].statement,
            "tends to work late around releases"
        );
    }

    #[test]
    fn empty_derived_list_parses() {
        assert!(parse_dream(r#"{"derived": []}"#)
            .unwrap()
            .derived
            .is_empty());
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_dream("nothing here").is_err());
    }
}
