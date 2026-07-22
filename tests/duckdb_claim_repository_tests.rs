//! Integration tests for the experimental append-only claim-graph store.
//!
//! Exercise the storage contract directly against an in-memory DuckDB: append
//! and read back, supersession semantics (status flip + `valid_to`), typed
//! edges, entity resolution by alias and by vector, project-scoped search, and
//! the forced-re-import hard delete.

use codesearch::{
    Claim, ClaimEdge, ClaimRepository, ClaimStatus, ClaimStoreStats, DuckdbClaimRepository,
    EdgeOrigin, EdgeType, Entity, EntityRef, SourceKind,
};

const DIMS: usize = 4;
const MODEL: &str = "mock-model";

fn store() -> DuckdbClaimRepository {
    DuckdbClaimRepository::in_memory(DIMS, MODEL).expect("claim store init")
}

/// A minimal active claim with the given id, statement, and project.
fn claim(id: &str, statement: &str, project: Option<&str>) -> Claim {
    Claim {
        id: id.to_string(),
        subject: EntityRef::Entity("alice".to_string()),
        predicate: "prefers".to_string(),
        object: EntityRef::Literal("tabs".to_string()),
        statement: statement.to_string(),
        project: project.map(str::to_string),
        recorded_at: 1000,
        valid_from: 1000,
        valid_to: None,
        source_session_id: Some("sess-1".to_string()),
        source_message_index: Some(3),
        source_kind: SourceKind::UserStated,
        confidence: 0.9,
        status: ClaimStatus::Active,
        derived: false,
        derived_from: Vec::new(),
    }
}

#[tokio::test]
async fn append_and_read_back_roundtrips_all_fields() {
    let repo = store();
    let mut c = claim("c1", "Alice prefers tabs", Some("svc-a"));
    c.object = EntityRef::Entity("tabs_entity".to_string());
    c.derived_from = vec!["p1".to_string(), "p2".to_string()];
    repo.append_claim(&c, Some(&[0.1, 0.2, 0.3, 0.4]))
        .await
        .unwrap();

    let got = repo.find_claim("c1").await.unwrap().expect("claim exists");
    assert_eq!(got, c, "claim must round-trip identically");
    assert_eq!(got.subject, EntityRef::Entity("alice".to_string()));
    assert_eq!(got.object, EntityRef::Entity("tabs_entity".to_string()));
    assert_eq!(got.derived_from, vec!["p1".to_string(), "p2".to_string()]);
}

#[tokio::test]
async fn literal_object_and_missing_provenance_roundtrip() {
    let repo = store();
    let mut c = claim("c1", "prefers tabs", None);
    c.source_session_id = None;
    c.source_message_index = None;
    c.valid_from = 500;
    repo.append_claim(&c, None).await.unwrap();

    let got = repo.find_claim("c1").await.unwrap().unwrap();
    assert_eq!(got.object, EntityRef::Literal("tabs".to_string()));
    assert_eq!(got.source_session_id, None);
    assert_eq!(got.source_message_index, None);
    assert_eq!(got.valid_from, 500);
    assert!(got.project.is_none());
}

#[tokio::test]
async fn supersession_flips_status_and_closes_validity() {
    let repo = store();
    let old = claim("old", "Alice lives in Berlin", None);
    let new = claim("new", "Alice lives in Munich", None);
    repo.append_claim(&old, None).await.unwrap();
    repo.append_claim(&new, None).await.unwrap();

    // A supersedes edge new -> old, and the old claim is retired non-destructively.
    repo.add_edge(&ClaimEdge {
        from_claim: "new".to_string(),
        to_claim: "old".to_string(),
        edge_type: EdgeType::Supersedes,
        created_at: 2000,
        created_by: EdgeOrigin::Ingestion,
        confidence: 1.0,
    })
    .await
    .unwrap();
    assert!(repo
        .set_claim_status("old", ClaimStatus::Superseded, Some(2000))
        .await
        .unwrap());

    let old_after = repo.find_claim("old").await.unwrap().unwrap();
    assert_eq!(old_after.status, ClaimStatus::Superseded);
    assert_eq!(old_after.valid_to, Some(2000));
    // The original claim is still present — append-only never deletes it.
    assert_eq!(old_after.statement, "Alice lives in Berlin");

    // Only the active claim is in the current-truth view.
    let active = repo
        .list_claims(Some(ClaimStatus::Active), None)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, "new");

    // Edges are navigable in both directions.
    let from_new = repo.edges_from("new").await.unwrap();
    assert_eq!(from_new.len(), 1);
    assert_eq!(from_new[0].edge_type, EdgeType::Supersedes);
    let to_old = repo.edges_to("old").await.unwrap();
    assert_eq!(to_old.len(), 1);
    assert_eq!(to_old[0].from_claim, "new");
}

#[tokio::test]
async fn retract_status_does_not_clear_existing_valid_to() {
    let repo = store();
    let c = claim("c1", "x", None);
    repo.append_claim(&c, None).await.unwrap();
    repo.set_claim_status("c1", ClaimStatus::Superseded, Some(2000))
        .await
        .unwrap();
    // Retract with no valid_to must leave the window intact.
    repo.set_claim_status("c1", ClaimStatus::Retracted, None)
        .await
        .unwrap();
    let got = repo.find_claim("c1").await.unwrap().unwrap();
    assert_eq!(got.status, ClaimStatus::Retracted);
    assert_eq!(got.valid_to, Some(2000));
}

#[tokio::test]
async fn set_status_on_missing_claim_returns_false() {
    let repo = store();
    assert!(!repo
        .set_claim_status("nope", ClaimStatus::Retracted, None)
        .await
        .unwrap());
}

#[tokio::test]
async fn entity_resolves_by_canonical_name_and_alias_case_insensitively() {
    let repo = store();
    let entity = Entity {
        id: "e1".to_string(),
        entity_type: "person".to_string(),
        canonical_name: "Alice".to_string(),
        aliases: vec![
            "my coworker Alice".to_string(),
            "Alice".to_string(), // duplicate of canonical — must not break the insert
            "  ".to_string(),    // blank — dropped
        ],
        created_at: 1,
        updated_at: 1,
    };
    repo.upsert_entity(&entity, Some(&[1.0, 0.0, 0.0, 0.0]))
        .await
        .unwrap();

    let by_canonical = repo.find_entity_by_alias("ALICE").await.unwrap().unwrap();
    assert_eq!(by_canonical.id, "e1");
    let by_alias = repo
        .find_entity_by_alias("my coworker alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_alias.id, "e1");
    assert!(repo.find_entity_by_alias("bob").await.unwrap().is_none());

    // Aliases round-trip (canonical duplicate and blank filtered out).
    let fetched = repo.find_entity("e1").await.unwrap().unwrap();
    assert_eq!(fetched.aliases, vec!["my coworker Alice".to_string()]);
}

#[tokio::test]
async fn upsert_entity_replaces_aliases() {
    let repo = store();
    let mut entity = Entity {
        id: "e1".to_string(),
        entity_type: "person".to_string(),
        canonical_name: "Alice".to_string(),
        aliases: vec!["ally".to_string()],
        created_at: 1,
        updated_at: 1,
    };
    repo.upsert_entity(&entity, None).await.unwrap();
    entity.aliases = vec!["al".to_string()];
    repo.upsert_entity(&entity, None).await.unwrap();

    assert!(repo.find_entity_by_alias("ally").await.unwrap().is_none());
    assert_eq!(
        repo.find_entity_by_alias("al").await.unwrap().unwrap().id,
        "e1"
    );
}

#[tokio::test]
async fn entity_semantic_search_ranks_by_similarity() {
    let repo = store();
    for (id, vec) in [
        ("near", [1.0, 0.0, 0.0, 0.0]),
        ("far", [0.0, 0.0, 0.0, 1.0]),
    ] {
        let entity = Entity {
            id: id.to_string(),
            entity_type: "thing".to_string(),
            canonical_name: id.to_string(),
            aliases: vec![],
            created_at: 1,
            updated_at: 1,
        };
        repo.upsert_entity(&entity, Some(&vec)).await.unwrap();
    }
    let hits = repo
        .search_entities_semantic(&[1.0, 0.0, 0.0, 0.0], 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.id, "near", "closest entity ranks first");
}

#[tokio::test]
async fn semantic_search_only_returns_active_claims_in_scope() {
    let repo = store();
    // Active, global.
    repo.append_claim(
        &claim("a", "duckdb locking", None),
        Some(&[1.0, 0.0, 0.0, 0.0]),
    )
    .await
    .unwrap();
    // Active, other project — must be excluded when scoped to svc-a.
    repo.append_claim(
        &claim("b", "duckdb locking", Some("svc-b")),
        Some(&[1.0, 0.0, 0.0, 0.0]),
    )
    .await
    .unwrap();
    // Superseded — must be excluded from current-truth search.
    let mut sup = claim("c", "duckdb locking", None);
    sup.status = ClaimStatus::Superseded;
    repo.append_claim(&sup, Some(&[1.0, 0.0, 0.0, 0.0]))
        .await
        .unwrap();

    let hits = repo
        .search_claims_semantic(&[1.0, 0.0, 0.0, 0.0], Some("svc-a"), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(c, _)| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["a"],
        "only the active, in-scope claim is returned"
    );
}

#[tokio::test]
async fn keyword_search_scores_by_term_overlap() {
    let repo = store();
    repo.append_claim(&claim("a", "duckdb lock conflict on writers", None), None)
        .await
        .unwrap();
    repo.append_claim(&claim("b", "unrelated preference about tabs", None), None)
        .await
        .unwrap();
    let hits = repo
        .search_claims_keyword("duckdb lock", None, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, "a");
}

#[tokio::test]
async fn delete_claims_for_session_hard_deletes_that_session_only() {
    let repo = store();
    let mut s1 = claim("s1c", "from session one", None);
    s1.source_session_id = Some("sess-1".to_string());
    let mut s2 = claim("s2c", "from session two", None);
    s2.source_session_id = Some("sess-2".to_string());
    repo.append_claim(&s1, Some(&[0.1, 0.2, 0.3, 0.4]))
        .await
        .unwrap();
    repo.append_claim(&s2, Some(&[0.4, 0.3, 0.2, 0.1]))
        .await
        .unwrap();
    // An edge touching the to-be-deleted claim must be cleaned up too.
    repo.add_edge(&ClaimEdge {
        from_claim: "s2c".to_string(),
        to_claim: "s1c".to_string(),
        edge_type: EdgeType::RelatesTo,
        created_at: 1,
        created_by: EdgeOrigin::Ingestion,
        confidence: 0.5,
    })
    .await
    .unwrap();

    let removed = repo.delete_claims_for_session("sess-1").await.unwrap();
    assert_eq!(removed, 1);
    assert!(repo.find_claim("s1c").await.unwrap().is_none());
    assert!(repo.find_claim("s2c").await.unwrap().is_some());
    // The dangling edge is gone.
    assert!(repo.edges_from("s2c").await.unwrap().is_empty());
    // And its vector no longer surfaces in search.
    let hits = repo
        .search_claims_semantic(&[0.1, 0.2, 0.3, 0.4], None, 10)
        .await
        .unwrap();
    assert!(hits.iter().all(|(c, _)| c.id != "s1c"));
}

#[tokio::test]
async fn stats_counts_claims_by_status_entities_and_edges() {
    let repo = store();
    repo.append_claim(&claim("a", "one", None), None)
        .await
        .unwrap();
    let mut sup = claim("b", "two", None);
    sup.status = ClaimStatus::Superseded;
    repo.append_claim(&sup, None).await.unwrap();
    repo.upsert_entity(
        &Entity {
            id: "e1".to_string(),
            entity_type: "person".to_string(),
            canonical_name: "Alice".to_string(),
            aliases: vec![],
            created_at: 1,
            updated_at: 1,
        },
        None,
    )
    .await
    .unwrap();
    repo.add_edge(&ClaimEdge {
        from_claim: "a".to_string(),
        to_claim: "b".to_string(),
        edge_type: EdgeType::Supersedes,
        created_at: 1,
        created_by: EdgeOrigin::Ingestion,
        confidence: 1.0,
    })
    .await
    .unwrap();

    let stats = repo.stats().await.unwrap();
    assert_eq!(
        stats,
        ClaimStoreStats {
            total_claims: 2,
            claims_by_status: vec![("active".to_string(), 1), ("superseded".to_string(), 1)],
            total_entities: 1,
            total_edges: 1,
        }
    );
}

#[tokio::test]
async fn reopening_with_different_dimensions_is_rejected() {
    // The meta guard only fires on a persistent file; an in-memory store starts
    // fresh each time, so assert the guard logic via a temp file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory-claims.duckdb");
    DuckdbClaimRepository::new(&path, DIMS, MODEL).expect("first open");
    let reopened = DuckdbClaimRepository::new(&path, DIMS + 1, MODEL);
    assert!(reopened.is_err(), "dimension mismatch must be rejected");
}
