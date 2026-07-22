//! Integration tests for the experimental claim-graph ingestion path:
//! transcript → scripted LLM extraction → entity resolution → DuckDB claim log.
//!
//! Uses an in-memory claim store, mock embeddings, and a scripted chat client,
//! so no network or model download is required.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use codesearch::{
    ChatClient, Claim, ClaimIngestionUseCase, ClaimRepository, ClaimStatus, DomainError,
    DuckdbClaimRepository, EdgeType, EmbeddingService, EntityRef, IngestionOutcome, MockEmbedding,
    SessionMessage, SessionTranscript, SourceKind,
};

/// Chat client that replays a fixed queue of responses.
struct ScriptedChatClient {
    responses: Mutex<Vec<String>>,
}

impl ScriptedChatClient {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
        }
    }
}

#[async_trait]
impl ChatClient for ScriptedChatClient {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, DomainError> {
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(DomainError::storage("no scripted response left"));
        }
        Ok(responses.remove(0))
    }
}

fn transcript(id: &str, project: Option<&str>, messages: &[(&str, &str)]) -> SessionTranscript {
    SessionTranscript {
        id: id.to_string(),
        source: format!("{id}.jsonl"),
        project: project.map(str::to_string),
        messages: messages
            .iter()
            .map(|(role, content)| SessionMessage {
                role: role.to_string(),
                content: content.to_string(),
                timestamp: None,
            })
            .collect(),
    }
}

fn use_case(
    responses: Vec<&str>,
) -> (
    ClaimIngestionUseCase,
    Arc<dyn ClaimRepository>,
    Arc<MockEmbedding>,
) {
    let embedding = Arc::new(MockEmbedding::new());
    let repo: Arc<dyn ClaimRepository> = Arc::new(
        DuckdbClaimRepository::in_memory(embedding.config().dimensions(), "mock-embedding")
            .expect("claim store"),
    );
    let chat = Arc::new(ScriptedChatClient::new(responses));
    let uc = ClaimIngestionUseCase::new(chat, Arc::clone(&repo), embedding.clone());
    (uc, repo, embedding)
}

#[tokio::test]
async fn ingests_claims_and_resolves_entities() {
    let response = r#"{"claims": [
        {"subject": "Alice", "subject_is_entity": true, "predicate": "lives_in",
         "object": "Munich", "object_is_entity": true,
         "statement": "Alice lives in Munich", "source_kind": "user_stated", "confidence": 0.9},
        {"subject": "Alice", "subject_is_entity": true, "predicate": "prefers",
         "object": "tabs", "object_is_entity": false,
         "statement": "Alice prefers tabs", "source_kind": "user_stated", "confidence": 0.8}
    ]}"#;
    let (uc, repo, _emb) = use_case(vec![response]);
    let t = transcript(
        "s1",
        None,
        &[("user", "I'm Alice, I moved to Munich; I like tabs")],
    );

    let outcome = uc.execute(&t, false).await.unwrap();
    let IngestionOutcome::Ingested(report) = outcome else {
        panic!("expected Ingested");
    };
    assert_eq!(report.claims_written, 2);
    // Alice + Munich are entities; tabs is a literal. Alice appears twice but is
    // created once.
    assert_eq!(report.entities_created, 2);
    assert_eq!(report.edges_added, 0);

    let active = repo
        .list_claims(Some(ClaimStatus::Active), None)
        .await
        .unwrap();
    assert_eq!(active.len(), 2);

    // The subject entity resolved and is reusable by alias.
    let alice = repo.find_entity_by_alias("alice").await.unwrap().unwrap();
    assert_eq!(alice.canonical_name, "Alice");
    // The literal object did not become an entity.
    assert!(repo.find_entity_by_alias("tabs").await.unwrap().is_none());

    // Both new claims carry the session as provenance.
    assert_eq!(repo.count_claims_for_session("s1").await.unwrap(), 2);
    let munich_claim = active.iter().find(|c| c.predicate == "lives_in").unwrap();
    assert!(matches!(munich_claim.object, EntityRef::Entity(_)));
    assert_eq!(munich_claim.source_kind, SourceKind::UserStated);
}

#[tokio::test]
async fn reuses_existing_entity_across_ingests() {
    let (uc, repo, _emb) = use_case(vec![
        r#"{"claims": [{"subject": "Alice", "subject_is_entity": true, "predicate": "uses",
            "object": "duckdb", "object_is_entity": true, "statement": "Alice uses duckdb",
            "source_kind": "assistant_inferred", "confidence": 0.7}]}"#,
    ]);
    let t = transcript("s1", None, &[("user", "Alice uses duckdb")]);
    let IngestionOutcome::Ingested(report) = uc.execute(&t, false).await.unwrap() else {
        panic!("expected Ingested");
    };
    // Two fresh entities: Alice, duckdb.
    assert_eq!(report.entities_created, 2);
    let alice_id = repo
        .find_entity_by_alias("alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let claims = repo.list_claims(None, None).await.unwrap();
    // The claim's subject points at the resolved Alice entity.
    assert_eq!(claims[0].subject, EntityRef::Entity(alice_id));
}

#[tokio::test]
async fn non_forced_reingest_is_skipped() {
    let response = r#"{"claims": [{"subject": "Alice", "subject_is_entity": true,
        "predicate": "prefers", "object": "spaces", "object_is_entity": false,
        "statement": "Alice prefers spaces", "source_kind": "user_stated", "confidence": 0.9}]}"#;
    // Only one scripted response — a second extraction call would error, proving
    // the second execute short-circuits before calling the model.
    let (uc, repo, _emb) = use_case(vec![response]);
    let t = transcript("s1", None, &[("user", "I prefer spaces")]);

    assert!(matches!(
        uc.execute(&t, false).await.unwrap(),
        IngestionOutcome::Ingested(_)
    ));
    assert!(matches!(
        uc.execute(&t, false).await.unwrap(),
        IngestionOutcome::AlreadyIngested
    ));
    assert_eq!(repo.count_claims_for_session("s1").await.unwrap(), 1);
}

#[tokio::test]
async fn forced_reimport_hard_deletes_prior_session_claims() {
    let first = r#"{"claims": [{"subject": "Alice", "subject_is_entity": true,
        "predicate": "prefers", "object": "spaces", "object_is_entity": false,
        "statement": "Alice prefers spaces", "source_kind": "user_stated", "confidence": 0.9}]}"#;
    let second = r#"{"claims": [
        {"subject": "Alice", "subject_is_entity": true, "predicate": "prefers",
         "object": "tabs", "object_is_entity": false, "statement": "Alice prefers tabs",
         "source_kind": "user_stated", "confidence": 0.9},
        {"subject": "Alice", "subject_is_entity": true, "predicate": "lives_in",
         "object": "Berlin", "object_is_entity": true, "statement": "Alice lives in Berlin",
         "source_kind": "user_stated", "confidence": 0.9}
    ]}"#;
    let (uc, repo, _emb) = use_case(vec![first, second]);
    let t = transcript("s1", None, &[("user", "prefs")]);

    uc.execute(&t, false).await.unwrap();
    assert_eq!(repo.count_claims_for_session("s1").await.unwrap(), 1);

    // Forced re-import wipes the session's prior claims and re-ingests fresh.
    let IngestionOutcome::Ingested(report) = uc.execute(&t, true).await.unwrap() else {
        panic!("expected Ingested");
    };
    assert_eq!(report.claims_written, 2);
    assert_eq!(repo.count_claims_for_session("s1").await.unwrap(), 2);
    // The stale "prefers spaces" claim is gone.
    let statements: Vec<String> = repo
        .list_claims(None, None)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.statement)
        .collect();
    assert!(!statements.iter().any(|s| s.contains("spaces")));
}

#[tokio::test]
async fn supersedes_relation_retires_prior_claim() {
    let (uc, repo, embedding) = use_case(vec![
        r#"{"claims": [{"subject": "Alice", "subject_is_entity": true, "predicate": "lives_in",
            "object": "Munich", "object_is_entity": true,
            "statement": "Alice lives in Munich now", "source_kind": "user_stated",
            "confidence": 0.95, "relation": {"type": "supersedes", "target": "old-1"}}]}"#,
    ]);

    // Pre-seed a prior active claim the model will supersede, with an embedding
    // so the prefetch surfaces it (and thus admits the relation target).
    let vector = embedding
        .embed_query("Alice lives in Berlin")
        .await
        .unwrap();
    let old = Claim {
        id: "old-1".to_string(),
        subject: EntityRef::Entity("e-alice".to_string()),
        predicate: "lives_in".to_string(),
        object: EntityRef::Literal("Berlin".to_string()),
        statement: "Alice lives in Berlin".to_string(),
        project: None,
        recorded_at: 100,
        valid_from: 100,
        valid_to: None,
        source_session_id: Some("s0".to_string()),
        source_message_index: None,
        source_kind: SourceKind::UserStated,
        confidence: 0.9,
        status: ClaimStatus::Active,
        derived: false,
        derived_from: Vec::new(),
    };
    repo.append_claim(&old, Some(&vector)).await.unwrap();

    let t = transcript(
        "s1",
        None,
        &[("user", "Actually Alice lives in Munich now")],
    );
    let IngestionOutcome::Ingested(report) = uc.execute(&t, false).await.unwrap() else {
        panic!("expected Ingested");
    };
    assert_eq!(report.claims_written, 1);
    assert_eq!(report.edges_added, 1);

    // The prior claim is retired non-destructively.
    let old_after = repo.find_claim("old-1").await.unwrap().unwrap();
    assert_eq!(old_after.status, ClaimStatus::Superseded);
    assert!(old_after.valid_to.is_some());

    // A supersedes edge points from the new claim to the old one.
    let edges = repo.edges_to("old-1").await.unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_type, EdgeType::Supersedes);

    // Current-truth view holds only the new claim.
    let active = repo
        .list_claims(Some(ClaimStatus::Active), None)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].statement, "Alice lives in Munich now");
}

#[tokio::test]
async fn relation_to_unknown_target_is_ignored() {
    // The model references a claim id that was never in the prefetch context —
    // the edge must be dropped rather than dangling.
    let (uc, _repo, _emb) = use_case(vec![
        r#"{"claims": [{"subject": "Alice", "subject_is_entity": true, "predicate": "lives_in",
            "object": "Munich", "object_is_entity": true, "statement": "Alice lives in Munich",
            "source_kind": "user_stated", "confidence": 0.9,
            "relation": {"type": "supersedes", "target": "does-not-exist"}}]}"#,
    ]);
    let t = transcript("s1", None, &[("user", "Alice lives in Munich")]);
    let IngestionOutcome::Ingested(report) = uc.execute(&t, false).await.unwrap() else {
        panic!("expected Ingested");
    };
    assert_eq!(report.claims_written, 1);
    assert_eq!(report.edges_added, 0);
}
