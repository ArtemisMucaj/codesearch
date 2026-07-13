//! Integration tests for the REST/JSON management API server skeleton
//! (`codesearch serve`'s management side).
//!
//! Boots the real management `Router` on an ephemeral port using an in-memory
//! container (memory storage + mock embeddings) so no external services or
//! persistent database are touched, then asserts `/health` behaves as
//! specified. No network egress: reqwest talks to our own loopback listener.

use std::sync::Arc;

use codesearch::{
    management_routes, Container, ContainerConfig, EmbeddingTarget, LlmTarget, ManagementAppState,
    RerankingTarget, VectorStore,
};
use tempfile::{tempdir, TempDir};

/// Build an in-memory container suitable for tests: memory storage, mock
/// embeddings, no reranking, no network.
///
/// Returns the `TempDir` guard alongside the container: the data directory
/// backs the lazily-opened `memory.duckdb`, so it must outlive the server (the
/// memory endpoints open it on first request).
async fn test_container() -> (Arc<Container>, TempDir) {
    let dir = tempdir().expect("failed to create temp dir");
    let config = ContainerConfig {
        data_dir: dir.path().to_string_lossy().to_string(),
        mock_embeddings: true,
        namespace: "search".to_string(),
        memory_storage: true,
        no_rerank: true,
        no_embeddings: false,
        read_only: false,
        expand_query: false,
        embedding_target: EmbeddingTarget::Onnx,
        reranking_target: RerankingTarget::Onnx,
        llm_target: LlmTarget::Anthropic,
        embedding_model: None,
        embedding_dimensions: 384,
        parse_concurrency: 1,
    };
    let container = Arc::new(
        Container::new(config)
            .await
            .expect("failed to build in-memory container"),
    );
    (container, dir)
}

/// Index a tiny Rust fixture into the container so repository/search endpoints
/// have data to return. Uses the same `index_use_case` the CLI drives.
async fn index_fixture(container: &Container) {
    let dir = tempdir().expect("failed to create fixture dir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("failed to create src dir");
    std::fs::write(
        src_dir.join("lib.rs"),
        r#"
/// Add two integers together.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#,
    )
    .expect("failed to write fixture file");

    container
        .index_use_case()
        .execute(
            dir.path().to_str().unwrap(),
            Some("fixture-repo"),
            VectorStore::InMemory,
            Some("search".to_string()),
            false,
        )
        .await
        .expect("failed to index fixture");
}

/// Boot the management router on an ephemeral port, returning its base URL, the
/// server task handle, and the container (so tests can index data first).
async fn spawn_management_server_with_container(
    container: Arc<Container>,
) -> (String, tokio::task::JoinHandle<()>) {
    let state = ManagementAppState::new(container);
    let app = management_routes(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    (format!("http://{addr}"), handle)
}

/// Boot the management router on an ephemeral port and return its base URL, the
/// server task handle (aborted at end of test), and the `TempDir` guard backing
/// the data directory (kept alive for the server's lifetime).
async fn spawn_management_server() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let (container, dir) = test_container().await;
    let state = ManagementAppState::new(container);
    let app = management_routes(state);

    // Port 0 lets the OS pick a free ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    (format!("http://{addr}"), handle, dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoint_returns_ok_with_version() {
    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::get(format!("{base_url}/health"))
        .await
        .expect("request to /health failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["status"], "ok");
    // The version must match the crate version compiled into the binary.
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_endpoint_describes_the_api() {
    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::get(format!("{base_url}/api"))
        .await
        .expect("request to /api failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["name"], "codesearch-management-api");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        body["endpoints"].is_array(),
        "index should list available endpoints"
    );
    // PR2 endpoints must be advertised in the index so clients can discover them.
    let paths: Vec<&str> = body["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["path"].as_str())
        .collect();
    for expected in [
        "/api/repositories",
        "/api/stats",
        "/api/search",
        "/api/impact",
        "/api/clusters",
        "/api/couplings",
        "/api/channels",
        "/api/memory",
    ] {
        assert!(
            paths.contains(&expected),
            "index should advertise {expected}, got {paths:?}"
        );
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn couplings_endpoint_returns_a_report() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    // Default level (file) — well-formed CouplingReport even for a tiny repo
    // with no fragile communities.
    let resp = reqwest::get(format!("{base_url}/api/couplings?repository=fixture-repo"))
        .await
        .expect("request to /api/couplings failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["level"], "file");
    assert!(body["total_communities"].is_number());
    assert!(body["fragile_communities"].is_number());
    assert!(body["communities"].is_array());

    // Explicit symbol level is accepted and reflected in the report.
    let resp = reqwest::get(format!(
        "{base_url}/api/couplings?repository=fixture-repo&level=symbol"
    ))
    .await
    .expect("symbol-level request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("symbol body was not JSON");
    assert_eq!(body["level"], "symbol");

    // An unknown level is a 400.
    let resp = reqwest::get(format!(
        "{base_url}/api/couplings?repository=fixture-repo&level=bogus"
    ))
    .await
    .expect("bad-level request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn repositories_endpoint_lists_indexed_repos() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    let resp = reqwest::get(format!("{base_url}/api/repositories"))
        .await
        .expect("request to /api/repositories failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    let repos = body["repositories"]
        .as_array()
        .expect("repositories should be an array");
    assert_eq!(repos.len(), 1, "one repository was indexed");
    assert_eq!(repos[0]["name"], "fixture-repo");
    assert!(
        repos[0]["file_count"].as_u64().unwrap() >= 1,
        "fixture repo should report indexed files"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_endpoint_reports_totals() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    let resp = reqwest::get(format!("{base_url}/api/stats"))
        .await
        .expect("request to /api/stats failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["repositories"], 1);
    assert!(body["total_files"].as_u64().unwrap() >= 1);
    assert_eq!(body["namespace"], "search");

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn search_endpoint_returns_results() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/api/search"))
        .json(&serde_json::json!({ "query": "add two integers", "limit": 5 }))
        .send()
        .await
        .expect("request to /api/search failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    let results = body["results"]
        .as_array()
        .expect("results should be an array");
    assert!(!results.is_empty(), "search should return at least one hit");
    let hit = &results[0];
    assert!(hit["file_path"].is_string(), "hit should carry a file_path");
    assert!(hit["score"].is_number(), "hit should carry a score");
    assert!(
        hit["content"].as_str().unwrap().contains("fn add"),
        "top hit should include the indexed function"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn repository_get_unknown_id_returns_404_json() {
    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::get(format!("{base_url}/api/repositories/does-not-exist"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert!(
        body["error"].is_string(),
        "errors should be JSON {{\"error\": ...}}, got {body}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_list_endpoint_returns_empty_shape() {
    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::get(format!("{base_url}/api/memory"))
        .await
        .expect("request to /api/memory failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["count"], 0);
    assert!(body["items"].is_array(), "items should be an array");

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn openapi_endpoint_returns_valid_json() {
    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::get(format!("{base_url}/api/openapi.json"))
        .await
        .expect("request to /api/openapi.json failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "expected JSON content type, got {content_type}"
    );

    // The body must parse as JSON and describe the streaming + skeleton paths.
    let doc: serde_json::Value = resp.json().await.expect("openapi body was not valid JSON");
    assert_eq!(doc["openapi"], "3.1.0");
    let paths = &doc["paths"];
    assert!(
        paths.get("/health").is_some(),
        "openapi should document /health"
    );
    assert!(
        paths.get("/api/stream/explain/{symbol}").is_some(),
        "openapi should document the explain stream endpoint"
    );
    assert!(
        paths.get("/api/stream/index").is_some(),
        "openapi should document the index stream endpoint"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_stream_emits_well_formed_sse_events() {
    // Build a tiny repository with a single source file so indexing has work to
    // do and terminates quickly. Everything runs against the in-memory
    // container — no external services, no network egress.
    let repo_dir = tempdir().expect("failed to create repo temp dir");
    std::fs::write(
        repo_dir.path().join("sample.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .expect("failed to write fixture file");

    let (base_url, server, _dir) = spawn_management_server().await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/stream/index"))
        .json(&serde_json::json!({ "path": repo_dir.path().to_string_lossy() }))
        .send()
        .await
        .expect("request to /api/stream/index failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "streaming endpoint must set the SSE content type, got {content_type}"
    );

    // The stream terminates on its own (start -> done), so reading the whole
    // body to completion is safe and non-blocking.
    let body = resp.text().await.expect("failed to read SSE body");

    // At least one well-formed SSE frame: a named event with a JSON data line.
    assert!(
        body.contains("event: progress"),
        "expected a `progress` event, got:\n{body}"
    );
    // A terminal frame — `done` on success, `error` on failure. Either is a
    // valid, well-formed SSE terminator; assert one of them is present.
    assert!(
        body.contains("event: done") || body.contains("event: error"),
        "expected a terminal `done`/`error` event, got:\n{body}"
    );

    // Verify the `data:` payload of the first event parses as JSON.
    let first_data = body
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .expect("SSE stream had no data line");
    let parsed: serde_json::Value =
        serde_json::from_str(first_data.trim()).expect("SSE data payload was not valid JSON");
    assert!(
        parsed.is_object(),
        "SSE data payload should be a JSON object"
    );

    server.abort();
}
