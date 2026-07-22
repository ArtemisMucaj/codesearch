//! Integration tests for claim-graph consolidation (the "dream" pass): clusters
//! near-duplicate primary claims and abstracts them into derived claims, without
//! ever modifying a primary. Scripted chat + mock embeddings, in-memory store.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use codesearch::{
    ChatClient, Claim, ClaimDreamUseCase, ClaimRepository, ClaimStatus, DomainError,
    DuckdbClaimRepository, EdgeType, EmbeddingService, EntityRef, MockEmbedding, SourceKind,
};

const DIMS: usize = 384;

struct ScriptedChatClient {
    responses: Mutex<Vec<String>>,
    calls: Mutex<usize>,
}

impl ScriptedChatClient {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            calls: Mutex::new(0),
        }
    }
    async fn call_count(&self) -> usize {
        *self.calls.lock().await
    }
}

#[async_trait]
impl ChatClient for ScriptedChatClient {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, DomainError> {
        *self.calls.lock().await += 1;
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(DomainError::storage("no scripted response left"));
        }
        Ok(responses.remove(0))
    }
}

fn setup(
    responses: Vec<&str>,
) -> (
    ClaimDreamUseCase,
    Arc<dyn ClaimRepository>,
    Arc<MockEmbedding>,
    Arc<ScriptedChatClient>,
) {
    let embedding = Arc::new(MockEmbedding::with_dimensions(DIMS));
    let repo: Arc<dyn ClaimRepository> =
        Arc::new(DuckdbClaimRepository::in_memory(DIMS, "mock-embedding").expect("claim store"));
    let chat = Arc::new(ScriptedChatClient::new(responses));
    let uc = ClaimDreamUseCase::new(Arc::clone(&repo), chat.clone(), embedding.clone());
    (uc, repo, embedding, chat)
}

fn claim(id: &str, statement: &str) -> Claim {
    Claim {
        id: id.to_string(),
        subject: EntityRef::Entity("user".to_string()),
        predicate: "did".to_string(),
        object: EntityRef::Literal("x".to_string()),
        statement: statement.to_string(),
        project: None,
        recorded_at: 1,
        valid_from: 1,
        valid_to: None,
        source_session_id: Some("s".to_string()),
        source_message_index: None,
        source_kind: SourceKind::UserStated,
        confidence: 0.9,
        status: ClaimStatus::Active,
        derived: false,
        derived_from: Vec::new(),
    }
}

/// Seed a cluster of claims that share a statement (identical mock embeddings →
/// cosine 1.0 → one cluster).
async fn seed_cluster(repo: &Arc<dyn ClaimRepository>, emb: &Arc<MockEmbedding>, ids: &[&str]) {
    for id in ids {
        let c = claim(id, "worked late before the release");
        let v = emb.embed_query(&c.statement).await.unwrap();
        repo.append_claim(&c, Some(&v)).await.unwrap();
    }
}

const DERIVE_RESPONSE: &str =
    r#"{"derived": [{"statement": "tends to work late around releases", "confidence": 0.95}]}"#;

#[tokio::test]
async fn abstracts_a_cluster_into_a_derived_claim() {
    let (uc, repo, emb, _chat) = setup(vec![DERIVE_RESPONSE]);
    seed_cluster(&repo, &emb, &["c1", "c2", "c3"]).await;

    let report = uc.execute().await.unwrap();
    assert_eq!(report.clusters_examined, 1);
    assert_eq!(report.derived_claims_added, 1);
    assert_eq!(report.edges_added, 3, "one refines edge per source claim");

    // The derived claim exists, is flagged, and links back to its sources.
    let all = repo.list_claims(None, None).await.unwrap();
    let derived: Vec<&Claim> = all.iter().filter(|c| c.derived).collect();
    assert_eq!(derived.len(), 1);
    let d = derived[0];
    assert_eq!(d.statement, "tends to work late around releases");
    assert_eq!(d.source_kind, SourceKind::Derived);
    assert_eq!(d.derived_from.len(), 3);
    // Derived confidence is capped below a primary observation.
    assert!(d.confidence <= 0.8, "derived confidence must be capped");

    // Refines edges point from each specific to the abstraction.
    let to_derived = repo.edges_to(&d.id).await.unwrap();
    assert_eq!(to_derived.len(), 3);
    assert!(to_derived.iter().all(|e| e.edge_type == EdgeType::Refines));

    // The primaries are untouched — still active, still primary.
    for id in ["c1", "c2", "c3"] {
        let c = repo.find_claim(id).await.unwrap().unwrap();
        assert_eq!(c.status, ClaimStatus::Active);
        assert!(!c.derived);
    }
}

#[tokio::test]
async fn second_pass_is_a_convergent_no_op() {
    // Only one scripted response: if the second pass called the model it would
    // error, proving convergence skips the already-abstracted cluster.
    let (uc, repo, emb, chat) = setup(vec![DERIVE_RESPONSE]);
    seed_cluster(&repo, &emb, &["c1", "c2", "c3"]).await;

    let first = uc.execute().await.unwrap();
    assert_eq!(first.derived_claims_added, 1);
    assert_eq!(chat.call_count().await, 1);

    let second = uc.execute().await.unwrap();
    assert_eq!(second.clusters_examined, 1);
    assert_eq!(
        second.clusters_skipped_stable, 1,
        "cluster already abstracted"
    );
    assert_eq!(second.derived_claims_added, 0);
    assert_eq!(chat.call_count().await, 1, "no second model call");

    // Still exactly one derived claim — no drift.
    let derived = repo
        .list_claims(None, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.derived)
        .count();
    assert_eq!(derived, 1);
}

#[tokio::test]
async fn singletons_are_not_clustered() {
    // Two claims with different statements → different mock vectors → no cluster.
    let (uc, repo, emb, chat) = setup(vec![]);
    let a = claim("a", "prefers tabs");
    let b = claim("b", "lives in a different city entirely");
    for c in [&a, &b] {
        let v = emb.embed_query(&c.statement).await.unwrap();
        repo.append_claim(c, Some(&v)).await.unwrap();
    }
    let report = uc.execute().await.unwrap();
    assert_eq!(report.clusters_examined, 0);
    assert_eq!(report.derived_claims_added, 0);
    assert_eq!(chat.call_count().await, 0, "no clusters, no model calls");
}

#[tokio::test]
async fn empty_derivation_adds_nothing() {
    let (uc, repo, emb, _chat) = setup(vec![r#"{"derived": []}"#]);
    seed_cluster(&repo, &emb, &["c1", "c2"]).await;
    let report = uc.execute().await.unwrap();
    assert_eq!(report.clusters_examined, 1);
    assert_eq!(report.derived_claims_added, 0);
    assert_eq!(report.edges_added, 0);
}
