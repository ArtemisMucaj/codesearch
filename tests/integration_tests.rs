use std::sync::Arc;

use codesearch::{
    CallGraphQuery, CallGraphRepository, CallGraphUseCase, CodeChunk, DuckdbCallGraphRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, FileHashRepository,
    InMemoryVectorRepository, IndexRepositoryUseCase, Language, ListRepositoriesUseCase,
    MockEmbedding, NodeType, ParserService, ReferenceKind, SearchCodeUseCase, SearchQuery,
    SymbolReference, TreeSitterParser, VectorStore,
};
use tempfile::tempdir;

async fn setup_test_env() -> TestEnv {
    let metadata_repository =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repository.shared_connection();
    let file_hash_repo: Arc<dyn FileHashRepository> = Arc::new(
        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create file hash repo"),
    );
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );
    let vector_repo = Arc::new(InMemoryVectorRepository::new());
    let parser = Arc::new(TreeSitterParser::new());

    let call_graph_use_case = Arc::new(CallGraphUseCase::new(call_graph_repo));

    TestEnv {
        metadata_repository,
        vector_repo,
        file_hash_repo,
        call_graph_use_case,
        parser,
    }
}

struct TestEnv {
    metadata_repository: Arc<DuckdbMetadataRepository>,
    #[allow(dead_code)]
    vector_repo: Arc<InMemoryVectorRepository>,
    #[allow(dead_code)]
    file_hash_repo: Arc<dyn FileHashRepository>,
    #[allow(dead_code)]
    call_graph_use_case: Arc<CallGraphUseCase>,
    #[allow(dead_code)]
    parser: Arc<TreeSitterParser>,
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_empty_repositories() {
    let env = setup_test_env().await;
    let use_case = ListRepositoriesUseCase::new(env.metadata_repository.clone());

    let repos = use_case
        .execute()
        .await
        .expect("Failed to list repositories");
    assert!(repos.is_empty(), "Should have no repositories initially");
}

#[tokio::test]
async fn test_parser_extracts_rust_functions() {
    let parser = TreeSitterParser::new();

    let code = r#"
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn multiply(x: f64, y: f64) -> f64 {
    x * y
}
"#;

    let chunks = parser
        .parse_file(code, "math.rs", Language::Rust, "test-repo")
        .await
        .expect("Failed to parse");

    assert!(!chunks.is_empty(), "Should extract functions");
}

#[tokio::test]
async fn test_parser_extracts_python_classes() {
    let parser = TreeSitterParser::new();

    let code = r#"
class Calculator:
    def __init__(self):
        self.value = 0

    def add(self, x):
        self.value += x
        return self

class StringHelper:
    @staticmethod
    def reverse(s):
        return s[::-1]
"#;

    let chunks = parser
        .parse_file(code, "helpers.py", Language::Python, "test-repo")
        .await
        .expect("Failed to parse");

    assert!(!chunks.is_empty(), "Should extract some chunks");

    let class_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| c.node_type() == NodeType::Class)
        .collect();

    assert_eq!(class_chunks.len(), 2, "Should extract 2 classes");
}

#[tokio::test]
async fn test_search_query_builder() {
    let query = SearchQuery::new("test query")
        .with_limit(20)
        .with_min_score(0.5)
        .with_languages(vec!["rust".to_string(), "python".to_string()])
        .with_repositories(vec!["repo1".to_string()]);

    assert_eq!(query.query(), "test query");
    assert_eq!(query.limit(), 20);
    assert_eq!(query.min_score(), Some(0.5));
    assert_eq!(
        query.languages(),
        Some(["rust".to_string(), "python".to_string()].as_slice())
    );
    assert_eq!(
        query.repository_ids(),
        Some(["repo1".to_string()].as_slice())
    );
}

#[tokio::test]
async fn test_language_detection() {
    use std::path::Path;

    assert_eq!(Language::from_path(Path::new("main.rs")), Language::Rust);
    assert_eq!(Language::from_path(Path::new("app.py")), Language::Python);
    assert_eq!(
        Language::from_path(Path::new("index.js")),
        Language::JavaScript
    );
    assert_eq!(
        Language::from_path(Path::new("app.tsx")),
        Language::TypeScript
    );
    assert_eq!(Language::from_path(Path::new("main.go")), Language::Go);
    assert_eq!(Language::from_path(Path::new("Main.kt")), Language::Kotlin);
    assert_eq!(
        Language::from_path(Path::new("build.gradle.kts")),
        Language::Kotlin
    );
    assert_eq!(
        Language::from_path(Path::new("readme.md")),
        Language::Unknown
    );
}

#[tokio::test]
async fn test_code_chunk_creation() {
    let chunk = CodeChunk::new(
        "src/main.rs".to_string(),
        "fn main() { }".to_string(),
        1,
        1,
        Language::Rust,
        NodeType::Function,
        "test-repo".to_string(),
    )
    .with_symbol_name("main");

    assert_eq!(chunk.file_path(), "src/main.rs");
    assert_eq!(chunk.symbol_name(), Some("main"));
    assert_eq!(chunk.language(), Language::Rust);
    assert_eq!(chunk.node_type(), NodeType::Function);
    assert_eq!(chunk.location(), "src/main.rs:1-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_vector_store_returns_chunk_documents() {
    let sqlite = Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = sqlite.shared_connection();
    let file_hash_repo: Arc<dyn FileHashRepository> = Arc::new(
        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create file hash repo"),
    );
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );
    let vector_repo = Arc::new(InMemoryVectorRepository::new());
    let parser = Arc::new(TreeSitterParser::new());
    let embedding_service = Arc::new(MockEmbedding::new());

    let call_graph_use_case = Arc::new(CallGraphUseCase::new(call_graph_repo));

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#,
    )
    .expect("Failed to write test file");

    let index_use_case = IndexRepositoryUseCase::new(
        sqlite.clone(),
        vector_repo.clone(),
        file_hash_repo,
        call_graph_use_case,
        parser,
        embedding_service.clone(),
    );

    index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("test-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    let search_use_case = SearchCodeUseCase::new(vector_repo, embedding_service);
    let query = SearchQuery::new("function that adds numbers").with_limit(3);
    let results = search_use_case.execute(query).await.expect("Search failed");

    assert!(!results.is_empty(), "Should find at least one result");
    assert!(
        results[0].chunk().content().contains("pub fn add"),
        "Top result should return chunk document content"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_end_to_end_index_and_search() {
    let env = setup_test_env().await;

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");

    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn subtract(a: i32, b: i32) -> i32 {
    a - b
}
"#,
    )
    .expect("Failed to write test file");

    let embedding_service = Arc::new(MockEmbedding::new());

    let index_use_case = IndexRepositoryUseCase::new(
        env.metadata_repository.clone(),
        env.vector_repo.clone(),
        env.file_hash_repo.clone(),
        env.call_graph_use_case.clone(),
        env.parser.clone(),
        embedding_service.clone(),
    );

    let repository = index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("test-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    assert!(
        repository.file_count() > 0,
        "Should have indexed at least one file"
    );
    assert!(
        repository.chunk_count() > 0,
        "Should have indexed at least one chunk"
    );

    let search_use_case = SearchCodeUseCase::new(env.vector_repo.clone(), embedding_service);

    let query = SearchQuery::new("function that adds numbers").with_limit(5);
    let results = search_use_case.execute(query).await.expect("Search failed");

    assert!(!results.is_empty(), "Should find at least one result");
    // MockEmbedding produces random unit vectors, so cosine similarity can be
    // negative. Assert only that the score is a real numeric value.
    assert!(
        results[0].score().is_finite(),
        "Top result should have a finite score, got {}",
        results[0].score()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hybrid_search_returns_results() {
    let env = setup_test_env().await;

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn compute(x: i32, y: i32) -> i32 {
    x + y
}

pub fn render(frame: &str) -> String {
    format!("frame: {}", frame)
}
"#,
    )
    .expect("Failed to write test file");

    let embedding_service = Arc::new(MockEmbedding::new());
    let index_use_case = IndexRepositoryUseCase::new(
        env.metadata_repository.clone(),
        env.vector_repo.clone(),
        env.file_hash_repo.clone(),
        env.call_graph_use_case.clone(),
        env.parser.clone(),
        embedding_service.clone(),
    );
    index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("hybrid-test-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    let search_use_case = SearchCodeUseCase::new(env.vector_repo.clone(), embedding_service);

    // text_search=true activates the hybrid (semantic + BM25 + RRF) path
    let query = SearchQuery::new("compute")
        .with_limit(5)
        .with_text_search(true);
    let results = search_use_case
        .execute(query)
        .await
        .expect("Hybrid search failed");

    assert!(!results.is_empty(), "Hybrid search should return results");
    // RRF scoring formula is 1/(RRF_K + rank) with RRF_K = 60.0, so fused
    // scores are always strictly positive regardless of the embedding model.
    assert!(
        results[0].score() > 0.0,
        "Hybrid results should have positive RRF scores, got {}",
        results[0].score()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hybrid_search_result_contains_matched_chunk() {
    // Verify that the chunk matching the keyword appears in the top results
    // when hybrid search is enabled.
    let env = setup_test_env().await;

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
pub fn authenticate(user: &str, password: &str) -> bool {
    user == "admin" && password == "secret"
}

pub fn compress_data(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}
"#,
    )
    .expect("Failed to write test file");

    let embedding_service = Arc::new(MockEmbedding::new());
    let index_use_case = IndexRepositoryUseCase::new(
        env.metadata_repository.clone(),
        env.vector_repo.clone(),
        env.file_hash_repo.clone(),
        env.call_graph_use_case.clone(),
        env.parser.clone(),
        embedding_service.clone(),
    );
    index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("keyword-test-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    let search_use_case = SearchCodeUseCase::new(env.vector_repo.clone(), embedding_service);

    let query = SearchQuery::new("authenticate")
        .with_limit(5)
        .with_text_search(true);
    let results = search_use_case.execute(query).await.expect("Search failed");

    assert!(!results.is_empty(), "Should find at least one result");
    // The authenticate function should appear in the results because it both
    // matches the text query and has a semantic embedding close to the query.
    let found = results
        .iter()
        .any(|r| r.chunk().content().contains("authenticate"));
    assert!(found, "authenticate chunk should appear in hybrid results");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hybrid_search_handles_special_chars_in_query() {
    // Queries containing SQL LIKE special characters (%, _, !) must not panic
    // or produce errors when text_search is enabled.
    let env = setup_test_env().await;

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");
    std::fs::write(src_dir.join("util.rs"), "pub fn helper() -> bool { true }")
        .expect("Failed to write test file");

    let embedding_service = Arc::new(MockEmbedding::new());
    let index_use_case = IndexRepositoryUseCase::new(
        env.metadata_repository.clone(),
        env.vector_repo.clone(),
        env.file_hash_repo.clone(),
        env.call_graph_use_case.clone(),
        env.parser.clone(),
        embedding_service.clone(),
    );
    index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("special-chars-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    let search_use_case = SearchCodeUseCase::new(env.vector_repo.clone(), embedding_service);

    // Each of these contains characters that are special in SQL LIKE patterns
    // or the escape character itself; none should cause an error.
    for special_query in &[
        "100% complete",
        "user_name filter",
        "path!to!file",
        "a%b_c!d",
        "score >= 0.5",
    ] {
        let query = SearchQuery::new(*special_query)
            .with_limit(5)
            .with_text_search(true);
        search_use_case
            .execute(query)
            .await
            .unwrap_or_else(|e| panic!("Hybrid search failed on query {:?}: {}", special_query, e));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hybrid_search_with_text_search_disabled_returns_semantic_only() {
    // Baseline: with text_search=false, results still come back (semantic path).
    // This ensures the flag correctly gates the BM25 leg.
    let env = setup_test_env().await;

    let temp_dir = tempdir().expect("Failed to create temp directory");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("Failed to create src directory");
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }",
    )
    .expect("Failed to write test file");

    let embedding_service = Arc::new(MockEmbedding::new());
    let index_use_case = IndexRepositoryUseCase::new(
        env.metadata_repository.clone(),
        env.vector_repo.clone(),
        env.file_hash_repo.clone(),
        env.call_graph_use_case.clone(),
        env.parser.clone(),
        embedding_service.clone(),
    );
    index_use_case
        .execute(
            temp_dir.path().to_str().unwrap(),
            Some("semantic-only-repo"),
            VectorStore::InMemory,
            None,
            false,
        )
        .await
        .expect("Indexing failed");

    let search_use_case = SearchCodeUseCase::new(env.vector_repo.clone(), embedding_service);

    let query = SearchQuery::new("add two numbers")
        .with_limit(5)
        .with_text_search(false);
    let results = search_use_case.execute(query).await.expect("Search failed");

    assert!(
        !results.is_empty(),
        "Semantic-only search should return results"
    );
    // MockEmbedding produces hash-seeded random unit vectors, so cosine similarity
    // is pseudo-random in [-1, 1] — no fixed threshold is reliable here.
    // We verify only that the score is a real numeric value; range correctness is
    // covered by the unit tests in in_memory_vector_repository.
    assert!(
        results[0].score().is_finite(),
        "Expected a finite cosine similarity score, got {}",
        results[0].score()
    );
}

/// Integration test: verifies that CommonJS `require()` bindings are found
/// when querying the call graph by the exported symbol name.
///
/// This test inserts mock SCIP-style references directly (bypassing the SCIP
/// binary which is not available in CI) and validates that `find_callers` and
/// the query logic work correctly with the data shapes the SCIP importer
/// produces.
///
/// Scenario (mirrors the user-reported bug):
///   - `sample_middleware.js` defines and exports `appApplicationSource`
///   - `sample_router.js` imports it as `const addSource = require(...)`
///
/// After the `normalize_symbol` fix, `callee_symbol` is stored as just
/// `appApplicationSource` (not the full SCIP path). The reference kind is
/// `Call` (inferred from the `().` descriptor suffix when SymbolKind is
/// Unspecified).
#[tokio::test(flavor = "multi_thread")]
async fn test_commonjs_require_captured_as_import_in_call_graph() {
    let env = setup_test_env().await;
    let repo_id = "cjs-test-repo";

    // Simulate the SCIP importer output for:
    //   const express = require('express')
    //   const addSource = require('./sample_middleware.js')
    //   app.use(addSource)  ← scip-typescript resolves this to appApplicationSource
    let refs = vec![
        // `const express = require('express')` — reference to the express module.
        // In real SCIP data, this is a plain reference (roles=0) to the module.
        // After normalize_symbol, the module name becomes `express`.
        SymbolReference::new(
            None, // no enclosing scope at module level
            "express".to_string(),
            "routes/sample_router.js".to_string(),
            "routes/sample_router.js".to_string(),
            1,
            18,
            ReferenceKind::Import,
            Language::JavaScript,
            repo_id.to_string(),
        ),
        // `app.use(addSource)` — scip-typescript resolves `addSource` to the
        // actual exported function `appApplicationSource`.
        SymbolReference::new(
            Some("setup".to_string()), // enclosing function
            "appApplicationSource".to_string(),
            "routes/sample_router.js".to_string(),
            "routes/sample_router.js".to_string(),
            5,
            12,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
        // `next()` inside appApplicationSource
        SymbolReference::new(
            Some("appApplicationSource".to_string()),
            "next".to_string(),
            "middlewares/sample_middleware.js".to_string(),
            "middlewares/sample_middleware.js".to_string(),
            3,
            3,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    // `const express = require('express')` → Import with callee "express"
    let express_callers = env
        .call_graph_use_case
        .find_callers("express", &query)
        .await
        .expect("find_callers failed for 'express'");

    let express_imports: Vec<_> = express_callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !express_imports.is_empty(),
        "Expected at least one Import reference with callee 'express' (from `const express = require('express')`)"
    );

    // `app.use(addSource)` → Call reference with callee "appApplicationSource"
    let app_src_callers = env
        .call_graph_use_case
        .find_callers("appApplicationSource", &query)
        .await
        .expect("find_callers failed for 'appApplicationSource'");

    assert!(
        !app_src_callers.is_empty(),
        "Expected callers for 'appApplicationSource' \
         (scip-typescript resolves `addSource` to the exported function)"
    );

    // `next()` inside appApplicationSource should be found
    let next_callers = env
        .call_graph_use_case
        .find_callers("next", &query)
        .await
        .expect("find_callers failed for 'next'");

    assert!(
        !next_callers.is_empty(),
        "Expected calls to 'next()' from appApplicationSource in sample_middleware.js"
    );
}

/// Integration test: verifies find_callers works when the middleware uses an
/// inline named function expression (`module.exports = function appApplicationSource(...) {}`).
///
/// Uses mock SCIP data directly instead of running the SCIP binary.
#[tokio::test(flavor = "multi_thread")]
async fn test_require_resolves_inline_named_function_export() {
    let env = setup_test_env().await;
    let repo_id = "inline-fn-export-test";

    // Simulate SCIP output for:
    //   middlewares/add-application-source.js:
    //     module.exports = function appApplicationSource(req, res, next) { next(); };
    //   routes/na-api-router.js:
    //     const addSource = require('../middlewares/add-application-source.js');
    //     function setup(app) { app.use(addSource); }
    //
    // scip-typescript resolves `addSource` at usage to `appApplicationSource`.
    let refs = vec![
        // Usage of addSource resolves to the exported function
        SymbolReference::new(
            Some("setup".to_string()),
            "appApplicationSource".to_string(),
            "routes/na-api-router.js".to_string(),
            "routes/na-api-router.js".to_string(),
            3,
            23,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
        // The require() line itself — import reference
        SymbolReference::new(
            None,
            "appApplicationSource".to_string(),
            "routes/na-api-router.js".to_string(),
            "routes/na-api-router.js".to_string(),
            1,
            18,
            ReferenceKind::Import,
            Language::JavaScript,
            repo_id.to_string(),
        )
        .with_import_alias("addSource"),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    let callers = env
        .call_graph_use_case
        .find_callers("appApplicationSource", &query)
        .await
        .expect("find_callers failed");

    let import_refs: Vec<_> = callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !import_refs.is_empty(),
        "find_callers('appApplicationSource') must find the import in routes/na-api-router.js \
         even when the middleware uses `module.exports = function appApplicationSource(...) {{}}`. \
         Got {} callers: {:?}",
        callers.len(),
        callers
            .iter()
            .map(|r| format!(
                "{}:{} callee={} alias={:?}",
                r.reference_file_path(),
                r.reference_line(),
                r.callee_symbol(),
                r.import_alias()
            ))
            .collect::<Vec<_>>()
    );
}

/// Integration test: `const addSource = require('../middlewares/add-application-source.js')`
/// where the middleware exports `appApplicationSource` should result in
/// `find_callers("appApplicationSource")` returning the import site — even though
/// the local binding is `addSource` (a different name, with `../` in the path).
///
/// Also verifies that `find_callers("addSource")` works via the import_alias UNION.
#[tokio::test(flavor = "multi_thread")]
async fn test_require_with_dotdot_path_resolves_to_exported_symbol() {
    let env = setup_test_env().await;
    let repo_id = "dotdot-require-test";

    // Simulate SCIP output:
    //   The require() creates an Import reference with callee=appApplicationSource
    //   and import_alias=addSource (the local binding name).
    let refs = vec![
        SymbolReference::new(
            None,
            "appApplicationSource".to_string(),
            "routes/na-api-router.js".to_string(),
            "routes/na-api-router.js".to_string(),
            2,
            18,
            ReferenceKind::Import,
            Language::JavaScript,
            repo_id.to_string(),
        )
        .with_import_alias("addSource"),
        // Usage of addSource at a call site, resolved to appApplicationSource
        SymbolReference::new(
            Some("setupRoutes".to_string()),
            "appApplicationSource".to_string(),
            "routes/na-api-router.js".to_string(),
            "routes/na-api-router.js".to_string(),
            5,
            24,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    // The primary assertion: find_callers("appApplicationSource") must return the
    // import in routes/na-api-router.js even though it was imported as `addSource`.
    let callers = env
        .call_graph_use_case
        .find_callers("appApplicationSource", &query)
        .await
        .expect("find_callers failed");

    let import_refs: Vec<_> = callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !import_refs.is_empty(),
        "Expected find_callers('appApplicationSource') to return the import from \
         routes/na-api-router.js (local binding: addSource). \
         Got {} total callers: {:?}",
        callers.len(),
        callers
            .iter()
            .map(|r| format!(
                "{}:{} callee={} alias={:?}",
                r.reference_file_path(),
                r.reference_line(),
                r.callee_symbol(),
                r.import_alias()
            ))
            .collect::<Vec<_>>()
    );

    // The alias must be preserved so that `find_callers("addSource")` still works too.
    let alias = import_refs[0].import_alias();
    assert_eq!(
        alias,
        Some("addSource"),
        "import_alias must be the local binding 'addSource'"
    );

    // Verify that searching by the alias also works (via the UNION on import_alias).
    let alias_callers = env
        .call_graph_use_case
        .find_callers("addSource", &query)
        .await
        .expect("find_callers by alias failed");

    assert!(
        !alias_callers.is_empty(),
        "find_callers('addSource') must also work via the import_alias UNION"
    );
}

/// Integration test: ES6 named import with alias is captured with the original
/// exported name as the callee and the local alias stored in import_alias.
///
/// Scenario:
///   `import { processRequest as handleReq } from './handler'`
///
/// `context processRequest` must find the import with import_alias = "handleReq".
#[tokio::test(flavor = "multi_thread")]
async fn test_es6_renamed_import_alias_recorded_in_call_graph() {
    let env = setup_test_env().await;
    let repo_id = "alias-test-repo";

    // Simulate SCIP output for ES6 named import with alias.
    // scip-typescript produces an Import role occurrence for this.
    let refs = vec![
        SymbolReference::new(
            None,
            "processRequest".to_string(),
            "consumer.ts".to_string(),
            "consumer.ts".to_string(),
            1,
            10,
            ReferenceKind::Import,
            Language::TypeScript,
            repo_id.to_string(),
        )
        .with_import_alias("handleReq"),
        // Usage of handleReq at a call site, resolved to processRequest
        SymbolReference::new(
            Some("main".to_string()),
            "processRequest".to_string(),
            "consumer.ts".to_string(),
            "consumer.ts".to_string(),
            4,
            5,
            ReferenceKind::Call,
            Language::TypeScript,
            repo_id.to_string(),
        ),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    // `find_callers("processRequest")` must return the import reference.
    let callers = env
        .call_graph_use_case
        .find_callers("processRequest", &query)
        .await
        .expect("find_callers failed");

    let import_refs: Vec<_> = callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !import_refs.is_empty(),
        "Expected an Import reference with callee 'processRequest' \
         from `import {{ processRequest as handleReq }}`"
    );

    // The import_alias must identify the local binding name.
    let alias_refs: Vec<_> = import_refs
        .iter()
        .filter(|r| r.import_alias() == Some("handleReq"))
        .collect();
    assert!(
        !alias_refs.is_empty(),
        "Expected import_alias = 'handleReq' on the Import reference for 'processRequest', \
         got aliases: {:?}",
        import_refs
            .iter()
            .map(|r| r.import_alias())
            .collect::<Vec<_>>()
    );
}

/// Integration test: CommonJS renamed destructure is captured correctly.
///
/// Scenario:
///   `const { createServer: makeServer } = require('http')`
///
/// `context createServer` must find the import with import_alias = "makeServer".
#[tokio::test(flavor = "multi_thread")]
async fn test_commonjs_renamed_destructure_alias_recorded_in_call_graph() {
    let env = setup_test_env().await;
    let repo_id = "cjs-destructure-test";

    // Simulate SCIP output for CommonJS destructured import with rename.
    let refs = vec![
        SymbolReference::new(
            None,
            "createServer".to_string(),
            "server.js".to_string(),
            "server.js".to_string(),
            1,
            8,
            ReferenceKind::Import,
            Language::JavaScript,
            repo_id.to_string(),
        )
        .with_import_alias("makeServer"),
        // Usage of makeServer — resolved to createServer
        SymbolReference::new(
            None,
            "createServer".to_string(),
            "server.js".to_string(),
            "server.js".to_string(),
            3,
            1,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    // Searching by original property name must find the import.
    let callers = env
        .call_graph_use_case
        .find_callers("createServer", &query)
        .await
        .expect("find_callers failed");

    let import_refs: Vec<_> = callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !import_refs.is_empty(),
        "Expected Import reference with callee 'createServer' \
         from `const {{ createServer: makeServer }} = require('http')`"
    );

    let alias_refs: Vec<_> = import_refs
        .iter()
        .filter(|r| r.import_alias() == Some("makeServer"))
        .collect();
    assert!(
        !alias_refs.is_empty(),
        "Expected import_alias = 'makeServer' on the Import reference for 'createServer', \
         got aliases: {:?}",
        import_refs
            .iter()
            .map(|r| r.import_alias())
            .collect::<Vec<_>>()
    );
}

/// Integration test: CommonJS shorthand destructure (no rename) is captured
/// with the property name as callee and import_alias = None.
///
/// Scenario: `const { createServer } = require('http')`
#[tokio::test(flavor = "multi_thread")]
async fn test_commonjs_shorthand_destructure_captured_without_alias() {
    let env = setup_test_env().await;
    let repo_id = "cjs-shorthand-test";

    // Simulate SCIP output for shorthand destructure (no alias).
    let refs = vec![
        SymbolReference::new(
            None,
            "createServer".to_string(),
            "server.js".to_string(),
            "server.js".to_string(),
            1,
            8,
            ReferenceKind::Import,
            Language::JavaScript,
            repo_id.to_string(),
        ),
        // Usage of createServer
        SymbolReference::new(
            None,
            "createServer".to_string(),
            "server.js".to_string(),
            "server.js".to_string(),
            3,
            1,
            ReferenceKind::Call,
            Language::JavaScript,
            repo_id.to_string(),
        ),
    ];

    env.call_graph_use_case
        .save_references(&refs)
        .await
        .expect("save_references failed");

    let query = CallGraphQuery::new().with_repository(repo_id);

    let callers = env
        .call_graph_use_case
        .find_callers("createServer", &query)
        .await
        .expect("find_callers failed");

    let import_refs: Vec<_> = callers
        .iter()
        .filter(|r| r.reference_kind() == ReferenceKind::Import)
        .collect();

    assert!(
        !import_refs.is_empty(),
        "Expected Import reference with callee 'createServer' \
         from `const {{ createServer }} = require('http')`"
    );

    assert_eq!(
        import_refs[0].import_alias(),
        None,
        "Shorthand destructure (no rename) must have import_alias = None"
    );
}
