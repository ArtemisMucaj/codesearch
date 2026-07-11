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
