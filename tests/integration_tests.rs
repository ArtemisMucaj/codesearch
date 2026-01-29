//! Integration tests for CodeSearch.
//!
//! These tests verify the end-to-end functionality of the system.

use std::sync::Arc;

use codesearch::{
    CodeChunk, InMemoryEmbeddingStorage, Language, ListRepositoriesUseCase, NodeType, ParserService,
    SearchQuery, SqliteStorage, TreeSitterParser,
};

/// Create an in-memory test environment.
async fn setup_test_env() -> TestEnv {
    let sqlite = Arc::new(SqliteStorage::in_memory().expect("Failed to create SQLite"));
    let embedding_repo = Arc::new(InMemoryEmbeddingStorage::new(sqlite.clone()));
    let parser = Arc::new(TreeSitterParser::new());

    TestEnv {
        sqlite,
        embedding_repo,
        parser,
    }
}

struct TestEnv {
    sqlite: Arc<SqliteStorage>,
    #[allow(dead_code)]
    embedding_repo: Arc<InMemoryEmbeddingStorage>,
    #[allow(dead_code)]
    parser: Arc<TreeSitterParser>,
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_empty_repositories() {
    let env = setup_test_env().await;
    let use_case = ListRepositoriesUseCase::new(env.sqlite.clone());

    let repos = use_case.execute().await.expect("Failed to list repositories");
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
        .filter(|c| c.node_type == NodeType::Class)
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

    assert_eq!(query.query, "test query");
    assert_eq!(query.limit, 20);
    assert_eq!(query.min_score, Some(0.5));
    assert_eq!(
        query.languages,
        Some(vec!["rust".to_string(), "python".to_string()])
    );
    assert_eq!(query.repository_ids, Some(vec!["repo1".to_string()]));
}

#[tokio::test]
async fn test_language_detection() {
    use std::path::Path;

    assert_eq!(Language::from_path(Path::new("main.rs")), Language::Rust);
    assert_eq!(Language::from_path(Path::new("app.py")), Language::Python);
    assert_eq!(Language::from_path(Path::new("index.js")), Language::JavaScript);
    assert_eq!(Language::from_path(Path::new("app.tsx")), Language::TypeScript);
    assert_eq!(Language::from_path(Path::new("main.go")), Language::Go);
    assert_eq!(Language::from_path(Path::new("readme.md")), Language::Unknown);
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

    assert_eq!(chunk.file_path, "src/main.rs");
    assert_eq!(chunk.symbol_name, Some("main".to_string()));
    assert_eq!(chunk.language, Language::Rust);
    assert_eq!(chunk.node_type, NodeType::Function);
    assert_eq!(chunk.location(), "src/main.rs:1-1");
}
