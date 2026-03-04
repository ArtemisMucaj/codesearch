use std::sync::Arc;

use codesearch::{
    CodeChunk, DuckdbVectorRepository, Embedding, Language, NamespaceEmbeddingConfig, NodeType,
    SearchQuery, VectorRepository,
};
use tempfile::tempdir;

fn unit_vector(dim: usize, hot_index: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[hot_index] = 1.0;
    v
}

fn default_cfg() -> NamespaceEmbeddingConfig {
    NamespaceEmbeddingConfig {
        embedding_target: "onnx".to_string(),
        embedding_model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
        dimensions: 384,
    }
}

/// Attempt to create an in-memory DuckdbVectorRepository.
/// Returns None (and prints a skip message) when the vss extension cannot be
/// installed, which happens in network-restricted CI environments.
fn try_in_memory() -> Option<Arc<DuckdbVectorRepository>> {
    match DuckdbVectorRepository::in_memory() {
        Ok(repo) => Some(Arc::new(repo)),
        Err(e) => {
            eprintln!("SKIP: DuckDB vss extension unavailable ({e}). Skipping test.");
            None
        }
    }
}

fn try_with_namespace(
    path: &std::path::Path,
    ns: &str,
    cfg: &NamespaceEmbeddingConfig,
) -> Option<Arc<DuckdbVectorRepository>> {
    match DuckdbVectorRepository::new_with_namespace(path, ns, cfg) {
        Ok(repo) => Some(Arc::new(repo)),
        Err(e) => {
            eprintln!("SKIP: DuckDB vss extension unavailable ({e}). Skipping test.");
            None
        }
    }
}

#[tokio::test]
async fn duckdb_vector_repository_can_save_and_search() {
    let Some(repo) = try_in_memory() else { return };

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
    let results: Vec<_> = repo.search(&embedding_vec, &query).await.expect("search");

    assert!(!results.is_empty(), "expected at least one result");
    assert_eq!(results[0].chunk().id(), chunk.id());
    assert!(results[0].score() > 0.99, "expected near-identical score");
}

#[tokio::test]
async fn duckdb_vector_repository_delete_by_repository_removes_all() {
    let Some(repo) = try_in_memory() else { return };

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
async fn duckdb_vector_repository_bm25_text_search_finds_matching_chunks() {
    // Verify that the DuckDB FTS-backed BM25 path finds chunks whose content
    // contains the query terms, even when the query embedding is unrelated to
    // the stored embedding (so semantic similarity alone would not find it).
    let Some(repo) = try_in_memory() else { return };

    let auth_chunk = CodeChunk::new(
        "src/auth.rs".to_string(),
        "pub fn authenticate_user(username: &str, password: &str) -> bool { \
         username == \"admin\" && password == \"secret\" }"
            .to_string(),
        1,
        3,
        Language::Rust,
        NodeType::Function,
        "repo-bm25".to_string(),
    )
    .with_symbol_name("authenticate_user");

    let unrelated_chunk = CodeChunk::new(
        "src/math.rs".to_string(),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-bm25".to_string(),
    )
    .with_symbol_name("add");

    let auth_emb = Embedding::new(
        auth_chunk.id().to_string(),
        unit_vector(384, 5),
        "mock".to_string(),
    );
    let math_emb = Embedding::new(
        unrelated_chunk.id().to_string(),
        unit_vector(384, 6),
        "mock".to_string(),
    );

    repo.save_batch(
        &[auth_chunk.clone(), unrelated_chunk],
        &[auth_emb, math_emb],
    )
    .await
    .expect("save_batch");

    // Query with a unit vector orthogonal to both stored vectors so semantic
    // scores are ~0; BM25 must carry the result.
    let query_vec = unit_vector(384, 42);
    let query = SearchQuery::new("authenticate user")
        .with_limit(5)
        .with_text_search(true);

    let results: Vec<_> = repo.search(&query_vec, &query).await.expect("BM25 search");

    assert!(!results.is_empty(), "BM25 search should return results");
    assert!(
        results
            .iter()
            .any(|r| r.chunk().content().contains("authenticate")),
        "authenticate_user chunk should appear in BM25 results"
    );
}

#[tokio::test]
async fn duckdb_vector_repository_bm25_handles_empty_query() {
    let Some(repo) = try_in_memory() else { return };

    let chunk = CodeChunk::new(
        "src/lib.rs".to_string(),
        "pub fn hello() {}".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "repo-empty".to_string(),
    );
    let emb = Embedding::new(
        chunk.id().to_string(),
        unit_vector(384, 0),
        "mock".to_string(),
    );
    repo.save_batch(&[chunk], &[emb]).await.expect("save_batch");

    // An empty query string should not panic or error.
    let query = SearchQuery::new("   ").with_limit(5).with_text_search(true);
    let results: Vec<_> = repo
        .search(&unit_vector(384, 0), &query)
        .await
        .expect("search with empty query");
    // Empty query produces no BM25 hits; result comes from semantic leg only.
    assert!(
        !results.is_empty(),
        "expected at least one result from semantic leg"
    );
    assert!(results[0].score().is_finite());
}

#[tokio::test]
async fn duckdb_vector_repository_schema_namespaces_tables() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");
    let cfg = default_cfg();

    let Some(repo_a) = try_with_namespace(&db_path, "schema_a", &cfg) else {
        return;
    };
    let Some(repo_b) = try_with_namespace(&db_path, "schema_b", &cfg) else {
        return;
    };

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
    let results_a: Vec<_> = repo_a
        .search(&embedding_vec, &query)
        .await
        .expect("search a");
    let results_b: Vec<_> = repo_b
        .search(&embedding_vec, &query)
        .await
        .expect("search b");

    assert_eq!(results_a.len(), 1);
    assert!(results_b.is_empty(), "expected no results from schema_b");
}
