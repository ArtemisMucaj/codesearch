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
    RerankingTarget,
};
use tempfile::tempdir;

/// Build an in-memory container suitable for tests: memory storage, mock
/// embeddings, no reranking, no network.
async fn test_container() -> Arc<Container> {
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
    Arc::new(
        Container::new(config)
            .await
            .expect("failed to build in-memory container"),
    )
}

/// Boot the management router on an ephemeral port and return its base URL plus
/// the server task handle (dropped at end of test to shut it down).
async fn spawn_management_server() -> (String, tokio::task::JoinHandle<()>) {
    let container = test_container().await;
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

    (format!("http://{addr}"), handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoint_returns_ok_with_version() {
    let (base_url, server) = spawn_management_server().await;

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
    let (base_url, server) = spawn_management_server().await;

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

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn openapi_endpoint_returns_valid_json() {
    let (base_url, server) = spawn_management_server().await;

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

    let (base_url, server) = spawn_management_server().await;

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
