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
    let results: Vec<_> = repo
        .search(Some(&embedding_vec), &query)
        .await
        .expect("search");

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

    let results: Vec<_> = repo
        .search(Some(&query_vec), &query)
        .await
        .expect("BM25 search");

    assert!(!results.is_empty(), "BM25 search should return results");
    assert!(
        results
            .iter()
            .any(|r| r.chunk().content().contains("authenticate")),
        "authenticate_user chunk should appear in BM25 results"
    );
}

/// Regression: building the FTS/BM25 index for a namespace whose name is not a
/// bare SQL identifier (e.g. `home-framework`) must succeed.
///
/// Previously the FTS source was a cross-schema view, which DuckDB's
/// `create_fts_index` cannot index — it failed with an opaque `out is null`
/// error at the end of indexing. Namespaces now back onto a generated schema
/// token (`ns_<hex>`), so the index is built directly on the real table
/// regardless of the user-facing namespace name. The chunks intentionally mix
/// present and NULL `symbol_name` values (as `--no-embeddings` SCIP-imported
/// chunks do) to exercise the nullable indexed column.
#[tokio::test]
async fn duckdb_vector_repository_bm25_builds_for_hyphenated_namespace() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("cs.duckdb");
    let cfg = default_cfg();

    let Some(repo) = try_with_namespace(&db_path, "home-framework", &cfg) else {
        return;
    };

    // One chunk with a symbol name, one without (NULL symbol_name) — both are
    // fed into the FTS index over `content` + `symbol_name`.
    let with_symbol = CodeChunk::new(
        "src/execution/engine.ts".to_string(),
        "export function runExecutionEngine(topology: Topology): void {}".to_string(),
        1,
        1,
        Language::TypeScript,
        NodeType::Function,
        "repo-hf".to_string(),
    )
    .with_symbol_name("runExecutionEngine");

    let without_symbol = CodeChunk::new(
        "src/execution/notes.ts".to_string(),
        "// orchestrates topology execution across the framework".to_string(),
        1,
        1,
        Language::TypeScript,
        NodeType::Block,
        "repo-hf".to_string(),
    );

    let with_emb = Embedding::new(
        with_symbol.id().to_string(),
        unit_vector(384, 3),
        "mock".to_string(),
    );
    let without_emb = Embedding::new(
        without_symbol.id().to_string(),
        unit_vector(384, 4),
        "mock".to_string(),
    );

    repo.save_batch(
        &[with_symbol.clone(), without_symbol],
        &[with_emb, without_emb],
    )
    .await
    .expect("save_batch");

    // `flush` builds the FTS index eagerly — this is the call site that raised
    // `out is null` before the fix. It must now succeed.
    repo.flush().await.expect("flush must build the FTS index");

    // And a BM25 query over the hyphenated namespace must return the match.
    let query_vec = unit_vector(384, 42);
    let query = SearchQuery::new("execution engine topology")
        .with_limit(5)
        .with_text_search(true);

    let results: Vec<_> = repo
        .search(Some(&query_vec), &query)
        .await
        .expect("BM25 search over hyphenated namespace");

    assert!(
        results
            .iter()
            .any(|r| r.chunk().content().contains("runExecutionEngine")),
        "expected the execution-engine chunk from a hyphenated namespace"
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
        .search(Some(&unit_vector(384, 0)), &query)
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
        .search(Some(&embedding_vec), &query)
        .await
        .expect("search a");
    let results_b: Vec<_> = repo_b
        .search(Some(&embedding_vec), &query)
        .await
        .expect("search b");

    assert_eq!(results_a.len(), 1);
    assert!(results_b.is_empty(), "expected no results from schema_b");
}

/// `create_namespace` persists the configuration, later reads resolve it, and
/// creating the same namespace twice is rejected.
#[test]
fn test_create_namespace_round_trip_and_duplicate() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");

    let cfg = NamespaceEmbeddingConfig {
        embedding_target: "api".to_string(),
        embedding_model: "my-custom-model".to_string(),
        dimensions: 768,
    };

    if let Err(e) = DuckdbVectorRepository::create_namespace(&db_path, "team-a", &cfg) {
        // vss extension unavailable in network-restricted environments
        eprintln!("SKIP: create_namespace unavailable ({e}). Skipping test.");
        return;
    }

    // The stored config is resolvable by namespace — this is what index/search
    // use to inherit the embedding setup without flags.
    let stored = codesearch::namespace_embedding_config(&db_path, "team-a")
        .expect("stored namespace config should resolve");
    assert_eq!(stored.embedding_target, "api");
    assert_eq!(stored.embedding_model, "my-custom-model");
    assert_eq!(stored.dimensions, 768);

    // Unknown namespaces resolve to nothing (callers fall back to defaults).
    assert!(codesearch::namespace_embedding_config(&db_path, "unknown").is_none());

    // A namespace's configuration is fixed at creation: re-creating errors.
    let err = DuckdbVectorRepository::create_namespace(&db_path, "team-a", &cfg)
        .expect_err("duplicate create should fail");
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );

    // Opening the namespace with the stored config validates cleanly.
    assert!(try_with_namespace(&db_path, "team-a", &cfg).is_some());
}

/// A namespace created with the no-embeddings sentinel is resolvable and
/// opens with any embedding config (nothing to validate against).
#[test]
fn test_create_no_embeddings_namespace() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");

    let cfg = NamespaceEmbeddingConfig {
        embedding_target: codesearch::NO_EMBEDDINGS_MODEL.to_string(),
        embedding_model: codesearch::NO_EMBEDDINGS_MODEL.to_string(),
        dimensions: 384,
    };

    if let Err(e) = DuckdbVectorRepository::create_namespace(&db_path, "fast", &cfg) {
        eprintln!("SKIP: create_namespace unavailable ({e}). Skipping test.");
        return;
    }

    let stored = codesearch::namespace_embedding_config(&db_path, "fast")
        .expect("stored namespace config should resolve");
    assert_eq!(stored.embedding_model, codesearch::NO_EMBEDDINGS_MODEL);

    // Opening with a default (embedding-enabled) config must not hard-error:
    // the sentinel skips embedding-space validation.
    assert!(try_with_namespace(&db_path, "fast", &default_cfg()).is_some());
}
