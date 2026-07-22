//! Integration tests for claim recall: graph-anchored hybrid retrieval over the
//! active-claim view, with 1-hop enrichment expansion.
//!
//! The expansion tests use `NoEmbedding` so the semantic leg is off and only the
//! keyword leg anchors — that way a neighbor with no shared query terms can
//! surface *only* via graph expansion, which is exactly what they assert.

use std::sync::Arc;

use codesearch::{
    Claim, ClaimEdge, ClaimRecallUseCase, ClaimRepository, ClaimStatus, DuckdbClaimRepository,
    EdgeOrigin, EdgeType, EmbeddingService, EntityRef, MockEmbedding, NoEmbedding, SourceKind,
};

const DIMS: usize = 384;

/// Recall over mock embeddings (semantic + keyword hybrid active).
fn hybrid_setup() -> (
    ClaimRecallUseCase,
    Arc<dyn ClaimRepository>,
    Arc<MockEmbedding>,
) {
    let embedding = Arc::new(MockEmbedding::with_dimensions(DIMS));
    let repo: Arc<dyn ClaimRepository> =
        Arc::new(DuckdbClaimRepository::in_memory(DIMS, "mock-embedding").expect("claim store"));
    let uc = ClaimRecallUseCase::new(Arc::clone(&repo), embedding.clone());
    (uc, repo, embedding)
}

/// Recall with embeddings off — keyword leg only, so expansion is the sole way
/// a term-mismatched neighbor can appear.
fn keyword_only_setup() -> (ClaimRecallUseCase, Arc<dyn ClaimRepository>) {
    let embedding = Arc::new(NoEmbedding::new(DIMS));
    let repo: Arc<dyn ClaimRepository> =
        Arc::new(DuckdbClaimRepository::in_memory(DIMS, "no-embeddings").expect("claim store"));
    let uc = ClaimRecallUseCase::new(Arc::clone(&repo), embedding);
    (uc, repo)
}

fn claim(id: &str, statement: &str, status: ClaimStatus) -> Claim {
    Claim {
        id: id.to_string(),
        subject: EntityRef::Entity("e1".to_string()),
        predicate: "p".to_string(),
        object: EntityRef::Literal("o".to_string()),
        statement: statement.to_string(),
        project: None,
        recorded_at: 1,
        valid_from: 1,
        valid_to: None,
        source_session_id: Some("s".to_string()),
        source_message_index: None,
        source_kind: SourceKind::UserStated,
        confidence: 0.9,
        status,
        derived: false,
        derived_from: Vec::new(),
    }
}

async fn append_embedded(repo: &Arc<dyn ClaimRepository>, emb: &Arc<MockEmbedding>, c: &Claim) {
    let v = emb.embed_query(&c.statement).await.unwrap();
    repo.append_claim(c, Some(&v)).await.unwrap();
}

async fn append_plain(repo: &Arc<dyn ClaimRepository>, c: &Claim) {
    repo.append_claim(c, None).await.unwrap();
}

fn ids(hits: &[(Claim, f32)]) -> Vec<String> {
    hits.iter().map(|(c, _)| c.id.clone()).collect()
}

#[tokio::test]
async fn keyword_leg_recalls_matching_claim() {
    let (uc, repo, emb) = hybrid_setup();
    append_embedded(
        &repo,
        &emb,
        &claim("a", "duckdb lock conflict on writers", ClaimStatus::Active),
    )
    .await;
    append_embedded(
        &repo,
        &emb,
        &claim("b", "user prefers tabs over spaces", ClaimStatus::Active),
    )
    .await;

    let hits = uc.execute("duckdb lock", None, 10).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.id, "a", "the lexical match ranks first");
}

#[tokio::test]
async fn superseded_claims_never_surface() {
    let (uc, repo, emb) = hybrid_setup();
    append_embedded(
        &repo,
        &emb,
        &claim("old", "server runs on port 8080", ClaimStatus::Superseded),
    )
    .await;
    append_embedded(
        &repo,
        &emb,
        &claim("new", "server runs on port 9090", ClaimStatus::Active),
    )
    .await;

    let hits = uc.execute("server port", None, 10).await.unwrap();
    let got = ids(&hits);
    assert!(got.iter().any(|i| i == "new"));
    assert!(
        !got.iter().any(|i| i == "old"),
        "superseded claim must not be recalled"
    );
}

#[tokio::test]
async fn expansion_pulls_refines_neighbor() {
    let (uc, repo) = keyword_only_setup();
    append_plain(&repo, &claim("parent", "has a pet", ClaimStatus::Active)).await;
    append_plain(
        &repo,
        &claim(
            "child",
            "the animal is a dog named Rex",
            ClaimStatus::Active,
        ),
    )
    .await;
    repo.add_edge(&ClaimEdge {
        from_claim: "child".to_string(),
        to_claim: "parent".to_string(),
        edge_type: EdgeType::Refines,
        created_at: 1,
        created_by: EdgeOrigin::Ingestion,
        confidence: 1.0,
    })
    .await
    .unwrap();

    let hits = uc.execute("has a pet", None, 10).await.unwrap();
    let got = ids(&hits);
    assert!(got.iter().any(|i| i == "parent"), "anchor present");
    assert!(
        got.iter().any(|i| i == "child"),
        "refines child pulled in by expansion"
    );
    let pos = |id: &str| got.iter().position(|x| x == id).unwrap();
    assert!(pos("parent") < pos("child"), "anchor outranks its neighbor");
}

#[tokio::test]
async fn expansion_does_not_surface_superseded_neighbor() {
    let (uc, repo) = keyword_only_setup();
    append_plain(
        &repo,
        &claim("anchor", "uses a database", ClaimStatus::Active),
    )
    .await;
    append_plain(
        &repo,
        &claim(
            "stale",
            "totally unrelated wording here",
            ClaimStatus::Superseded,
        ),
    )
    .await;
    repo.add_edge(&ClaimEdge {
        from_claim: "anchor".to_string(),
        to_claim: "stale".to_string(),
        edge_type: EdgeType::RelatesTo,
        created_at: 1,
        created_by: EdgeOrigin::Ingestion,
        confidence: 1.0,
    })
    .await
    .unwrap();

    let hits = uc.execute("uses a database", None, 10).await.unwrap();
    let got = ids(&hits);
    assert!(got.iter().any(|i| i == "anchor"));
    assert!(
        !got.iter().any(|i| i == "stale"),
        "superseded neighbor must be filtered"
    );
}

#[tokio::test]
async fn supersedes_edge_is_not_walked_for_expansion() {
    let (uc, repo) = keyword_only_setup();
    append_plain(
        &repo,
        &claim("anchor", "config value is X", ClaimStatus::Active),
    )
    .await;
    append_plain(
        &repo,
        &claim("other", "nothing lexically shared zzz", ClaimStatus::Active),
    )
    .await;
    repo.add_edge(&ClaimEdge {
        from_claim: "anchor".to_string(),
        to_claim: "other".to_string(),
        edge_type: EdgeType::Supersedes,
        created_at: 1,
        created_by: EdgeOrigin::Ingestion,
        confidence: 1.0,
    })
    .await
    .unwrap();

    let hits = uc.execute("config value", None, 10).await.unwrap();
    let got = ids(&hits);
    assert!(got.iter().any(|i| i == "anchor"));
    assert!(
        !got.iter().any(|i| i == "other"),
        "a supersedes edge is not an enrichment edge and must not be walked"
    );
}

#[tokio::test]
async fn empty_query_is_rejected() {
    let (uc, _repo) = keyword_only_setup();
    assert!(uc.execute("   ", None, 10).await.is_err());
}
