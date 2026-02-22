use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, CodeChunk, DuckdbCallGraphRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, FileHashRepository,
    InMemoryVectorRepository, IndexRepositoryUseCase, Language, ListRepositoriesUseCase,
    MockEmbedding, NodeType, ParserService, SearchCodeUseCase, SearchQuery, TreeSitterParser,
    VectorStore,
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

    // Create CallGraphUseCase with parser-based extractor
    let call_graph_use_case = Arc::new(CallGraphUseCase::with_parser(
        parser.clone() as Arc<dyn ParserService>,
        call_graph_repo,
    ));

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
    assert_eq!(
        Language::from_path(Path::new("Main.kt")),
        Language::Kotlin
    );
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

    // Create CallGraphUseCase with parser-based extractor
    let call_graph_use_case = Arc::new(CallGraphUseCase::with_parser(
        parser.clone() as Arc<dyn ParserService>,
        call_graph_repo,
    ));

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
    assert!(
        results[0].score() > 0.0,
        "Top result should have positive score"
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
    let query = SearchQuery::new("compute").with_limit(5).with_text_search(true);
    let results = search_use_case
        .execute(query)
        .await
        .expect("Hybrid search failed");

    assert!(!results.is_empty(), "Hybrid search should return results");
    assert!(
        results[0].score() > 0.0,
        "Hybrid results should have positive RRF scores"
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
    std::fs::write(
        src_dir.join("util.rs"),
        "pub fn helper() -> bool { true }",
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

    assert!(!results.is_empty(), "Semantic-only search should return results");
    // Cosine-based scores are significantly higher than RRF scores
    assert!(
        results[0].score() > 0.1,
        "Semantic scores should be larger than RRF scores"
    );
}
