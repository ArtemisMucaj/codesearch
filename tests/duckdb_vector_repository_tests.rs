use std::sync::Arc;

use codesearch::{
    CodeChunk, DuckdbVectorRepository, Embedding, Language, NodeType, SearchQuery, VectorRepository,
};
use tempfile::tempdir;

fn unit_vector(dim: usize, hot_index: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[hot_index] = 1.0;
    v
}

#[tokio::test]
async fn duckdb_vector_repository_can_save_and_search() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("vectors.duckdb");

    let repo = Arc::new(DuckdbVectorRepository::new(&db_path).expect("duckdb init"));

    let chunk = CodeChunk::new(
        "src/lib.rs".to_string(),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-1".to_string(),
    )
    .with_symbol_name("add");

    let embedding_vec = unit_vector(384, 0);
    let embedding = Embedding::new(
        chunk.id().to_string(),
        embedding_vec.clone(),
        "mock".to_string(),
    );

    repo.save_batch(std::slice::from_ref(&chunk), &[embedding])
        .await
        .expect("save_batch");

    let query = SearchQuery::new("add numbers").with_limit(3);
    let results = repo.search(&embedding_vec, &query).await.expect("search");

    assert!(!results.is_empty(), "expected at least one result");
    assert_eq!(results[0].chunk().id(), chunk.id());
    assert!(results[0].score() > 0.99, "expected near-identical score");
}

#[tokio::test]
async fn duckdb_vector_repository_delete_by_repository_removes_all() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("vectors.duckdb");

    let repo = Arc::new(DuckdbVectorRepository::new(&db_path).expect("duckdb init"));

    let chunk1 = CodeChunk::new(
        "src/a.rs".to_string(),
        "fn a() {}".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-del".to_string(),
    );
    let chunk2 = CodeChunk::new(
        "src/b.rs".to_string(),
        "fn b() {}".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-del".to_string(),
    );

    let e1 = Embedding::new(
        chunk1.id().to_string(),
        unit_vector(384, 1),
        "mock".to_string(),
    );
    let e2 = Embedding::new(
        chunk2.id().to_string(),
        unit_vector(384, 2),
        "mock".to_string(),
    );

    repo.save_batch(&[chunk1, chunk2], &[e1, e2])
        .await
        .expect("save_batch");
    assert_eq!(repo.count().await.expect("count"), 2);

    repo.delete_by_repository("repo-del")
        .await
        .expect("delete_by_repository");

    assert_eq!(repo.count().await.expect("count"), 0);
}

#[tokio::test]
async fn duckdb_vector_repository_schema_namespaces_tables() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");

    let repo_a = Arc::new(
        DuckdbVectorRepository::new_with_namespace(&db_path, "schema_a").expect("duckdb init a"),
    );
    let repo_b = Arc::new(
        DuckdbVectorRepository::new_with_namespace(&db_path, "schema_b").expect("duckdb init b"),
    );

    let chunk = CodeChunk::new(
        "src/lib.rs".to_string(),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-1".to_string(),
    )
    .with_symbol_name("add");
    let embedding_vec = unit_vector(384, 0);
    let embedding = Embedding::new(
        chunk.id().to_string(),
        embedding_vec.clone(),
        "mock".to_string(),
    );

    repo_a
        .save_batch(std::slice::from_ref(&chunk), &[embedding])
        .await
        .expect("save_batch");

    assert_eq!(repo_a.count().await.expect("count a"), 1);
    assert_eq!(repo_b.count().await.expect("count b"), 0);

    let query = SearchQuery::new("add numbers").with_limit(3);
    let results_a = repo_a
        .search(&embedding_vec, &query)
        .await
        .expect("search a");
    let results_b = repo_b
        .search(&embedding_vec, &query)
        .await
        .expect("search b");

    assert_eq!(results_a.len(), 1);
    assert!(results_b.is_empty(), "expected no results from schema_b");
}
