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

/// Build an in-memory container suitable for tests: in-memory vector storage,
/// mock embeddings, no reranking, no network.
///
/// Returns the `TempDir` guard alongside the container: the data directory
/// backs the DuckDB metadata store, so it must outlive the server.
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

/// Like [`test_container`] but backed by DuckDB, for the tests that need real
/// namespaces — in-memory storage has none.
async fn duckdb_container() -> (Arc<Container>, TempDir) {
    let dir = tempdir().expect("failed to create temp dir");
    let config = ContainerConfig {
        data_dir: dir.path().to_string_lossy().to_string(),
        mock_embeddings: true,
        namespace: "search".to_string(),
        memory_storage: false,
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
            .expect("failed to build DuckDB-backed container"),
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
        "/api/graph",
        "/api/couplings",
        "/api/channels",
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
async fn graph_endpoint_returns_a_graph_view() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    // Default level (file) — a well-formed GraphView with the edge adjacency
    // the /api/clusters endpoints omit.
    let resp = reqwest::get(format!("{base_url}/api/graph?repository=fixture-repo"))
        .await
        .expect("request to /api/graph failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    assert_eq!(body["level"], "file");
    assert!(body["nodes"].is_array());
    assert!(body["edges"].is_array());
    assert!(body["communities"].is_array());

    // Explicit symbol level is accepted and reflected in the view.
    let resp = reqwest::get(format!(
        "{base_url}/api/graph?repository=fixture-repo&level=symbol"
    ))
    .await
    .expect("symbol-level request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("symbol body was not JSON");
    assert_eq!(body["level"], "symbol");

    // An unknown level is a 400.
    let resp = reqwest::get(format!(
        "{base_url}/api/graph?repository=fixture-repo&level=bogus"
    ))
    .await
    .expect("bad-level request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn clusters_and_graph_endpoints_support_global_scope() {
    let (container, _dir) = test_container().await;
    index_fixture(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    // Namespace-wide clusters: cached under the sentinel scope id.
    let resp = reqwest::get(format!("{base_url}/api/clusters?global=true"))
        .await
        .expect("request to /api/clusters?global=true failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("response body was not JSON");
    // The sentinel scope id is namespace-qualified so different namespaces' global
    // partitions never collide in the (namespace-less) analysis cache.
    assert_eq!(
        body["repository_id"],
        codesearch::namespace_scope_id("search")
    );
    assert!(body["clusters"].is_array());

    // Namespace-wide graph view at the (default) file level.
    let resp = reqwest::get(format!("{base_url}/api/graph?global=true"))
        .await
        .expect("request to /api/graph?global=true failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("graph body was not JSON");
    assert_eq!(
        body["repository_id"],
        codesearch::namespace_scope_id("search")
    );
    assert_eq!(body["level"], "file");

    // Namespace-wide graph at the symbol level: one Leiden run over every
    // repository's symbols, cross-repo call edges included.
    let resp = reqwest::get(format!("{base_url}/api/graph?global=true&level=symbol"))
        .await
        .expect("request to /api/graph?global=true&level=symbol failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("symbol graph body was not JSON");
    assert_eq!(
        body["repository_id"],
        codesearch::namespace_scope_id("search")
    );
    assert_eq!(body["level"], "symbol");

    // `/api/symbol-clusters` now has a global form too, scoped the same way as
    // the file level, so the structured community list matches what
    // `/api/graph?level=symbol&global=true` renders.
    let resp = reqwest::get(format!(
        "{base_url}/api/symbol-clusters?global=true&namespace=search"
    ))
    .await
    .expect("global symbol-clusters request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("global symbol-clusters body was not JSON");
    assert_eq!(
        body["repository_id"],
        codesearch::namespace_scope_id("search")
    );

    // Conflicting scope selectors are still 400s.
    for path in [
        "/api/clusters?global=true&repository=fixture-repo",
        "/api/graph?global=true&repository=fixture-repo",
        "/api/symbol-clusters?global=true&repository=fixture-repo",
    ] {
        let resp = reqwest::get(format!("{base_url}{path}"))
            .await
            .expect("bad-combination request failed");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "expected 400 for {path}"
        );
        let body: serde_json::Value = resp.json().await.expect("error body was not JSON");
        assert!(body["error"].is_string(), "error shape for {path}: {body}");
    }

    server.abort();
}

/// Index the two messaging fixtures as separate repositories, so channel tests
/// have a producer in one repo and a consumer in another. Returns each one's
/// `(name, id)` in `(producer, consumer)` order — the query filter takes names
/// while the report keys endpoints by repository id.
async fn index_messaging_fixtures(container: &Container) -> ((String, String), (String, String)) {
    // Both fixtures are Python: producing a channel graph must not depend on an
    // external indexer being installed. The JS notification fixture would drag
    // in `scip-typescript`, which CI does not have on PATH.
    for (path, name) in [
        ("tests/fixtures/messaging/orders-service", "orders-service"),
        (
            "tests/fixtures/messaging/notifications-py",
            "notifications-py",
        ),
    ] {
        container
            .index_use_case()
            .execute(
                path,
                Some(name),
                VectorStore::InMemory,
                Some("search".to_string()),
                false,
            )
            .await
            .unwrap_or_else(|e| panic!("failed to index {name}: {e}"));
    }

    let repos = container
        .metadata_repository()
        .list()
        .await
        .expect("failed to list indexed repositories");
    let id_of = |name: &str| {
        repos
            .iter()
            .find(|r| r.name() == name)
            .map(|r| (name.to_string(), r.id().to_string()))
            .unwrap_or_else(|| panic!("{name} was not indexed"))
    };
    (id_of("orders-service"), id_of("notifications-py"))
}

/// `GET /api/channels[?query]`, asserting the response is a well-formed report
/// and returning its body.
async fn channels(base_url: &str, query: &str) -> serde_json::Value {
    let url = if query.is_empty() {
        format!("{base_url}/api/channels")
    } else {
        format!("{base_url}/api/channels?{query}")
    };
    let resp = reqwest::get(url)
        .await
        .expect("request to /api/channels failed");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "channels should accept `{query}`"
    );
    let body: serde_json::Value = resp.json().await.expect("channels body was not JSON");
    assert!(body["edges"].is_array());
    assert!(body["unmatched_producers"].is_array());
    assert!(body["unmatched_consumers"].is_array());
    body
}

/// Collect the set of repository ids appearing anywhere in a channels report,
/// so a filter can be checked for actually excluding the repositories it omits.
fn repositories_in_report(body: &serde_json::Value) -> std::collections::BTreeSet<String> {
    let mut repos = std::collections::BTreeSet::new();
    let mut record = |endpoint: &serde_json::Value| {
        if let Some(repo) = endpoint["repository_id"].as_str() {
            repos.insert(repo.to_string());
        }
    };
    for key in ["unmatched_producers", "unmatched_consumers"] {
        for endpoint in body[key].as_array().into_iter().flatten() {
            record(endpoint);
        }
    }
    for edge in body["edges"].as_array().into_iter().flatten() {
        for side in ["producer", "consumer"] {
            record(&edge[side]);
        }
    }
    repos
}

#[tokio::test(flavor = "multi_thread")]
async fn channels_endpoint_accepts_comma_separated_repository_filter() {
    let (container, _dir) = test_container().await;
    let ((producer_repo, producer_id), (consumer_repo, consumer_id)) =
        index_messaging_fixtures(&container).await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    // Unfiltered (cwd namespace) sees both services.
    let all = repositories_in_report(&channels(&base_url, "").await);
    assert!(
        all.contains(&producer_id) && all.contains(&consumer_id),
        "unfiltered report should span both services, got {all:?}"
    );

    // The `repository` filter is a comma-separated string (a Vec can't be
    // deserialized from a query key). Before the fix the param failed to
    // deserialize and the filter was silently dropped, leaking every
    // repository's channels — so assert the filtered report actually EXCLUDES
    // the repository it does not name, not merely that it is well-shaped.
    let filtered =
        repositories_in_report(&channels(&base_url, &format!("repository={producer_repo}")).await);
    assert!(
        !filtered.contains(&consumer_id),
        "filtering on `{producer_repo}` must exclude `{consumer_repo}`, got {filtered:?}"
    );

    // A comma list binds too, and naming both repositories restores both.
    let both = repositories_in_report(
        &channels(
            &base_url,
            &format!("repository={producer_repo},{consumer_repo}"),
        )
        .await,
    );
    assert_eq!(
        both, all,
        "naming both repositories should match the unfiltered report"
    );

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

#[tokio::test(flavor = "multi_thread")]
async fn index_stream_treats_a_null_namespace_as_absent() {
    // The CLI serializes an unset `--namespace` as JSON `null` rather than
    // omitting the key. That has to read as "no override" — treating it as one
    // would reject every routed `index` against an in-memory server.
    let repo_dir = tempdir().expect("failed to create repo temp dir");
    std::fs::write(repo_dir.path().join("sample.rs"), "pub fn a() {}\n")
        .expect("failed to write fixture file");

    let (base_url, server, _dir) = spawn_management_server().await;

    let body = reqwest::Client::new()
        .post(format!("{base_url}/api/stream/index"))
        .json(&serde_json::json!({
            "path": repo_dir.path().to_string_lossy(),
            "namespace": serde_json::Value::Null,
        }))
        .send()
        .await
        .expect("request to /api/stream/index failed")
        .text()
        .await
        .expect("failed to read SSE body");

    assert!(
        body.contains("event: done"),
        "a null namespace must behave as if absent, got:\n{body}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_stream_rejects_a_namespace_in_memory() {
    // In-memory storage has no namespaces, so an override there cannot be
    // honoured. Failing loudly beats indexing somewhere the caller didn't ask
    // for, which is what silently ignoring the field would do.
    let repo_dir = tempdir().expect("failed to create repo temp dir");
    std::fs::write(
        repo_dir.path().join("sample.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .expect("failed to write fixture file");

    let (base_url, server, _dir) = spawn_management_server().await;

    let body = reqwest::Client::new()
        .post(format!("{base_url}/api/stream/index"))
        .json(&serde_json::json!({
            "path": repo_dir.path().to_string_lossy(),
            "namespace": "platform",
        }))
        .send()
        .await
        .expect("request to /api/stream/index failed")
        .text()
        .await
        .expect("failed to read SSE body");

    assert!(
        body.contains("event: error"),
        "expected an `error` event, got:\n{body}"
    );
    assert!(
        !body.contains("event: done"),
        "indexing must not complete when the namespace cannot be honoured:\n{body}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_stream_indexes_into_a_new_namespace() {
    // The flow a UI drives: create a namespace, then index into it. Before the
    // request carried a namespace this was impossible without restarting the
    // server, since indexing always used the one `serve` started with.
    let repo_dir = tempdir().expect("failed to create repo temp dir");
    std::fs::write(
        repo_dir.path().join("sample.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .expect("failed to write fixture file");

    let (container, _dir) = duckdb_container().await;
    let (base_url, server) = spawn_management_server_with_container(container).await;
    let client = reqwest::Client::new();

    let create = client
        .post(format!("{base_url}/api/namespaces"))
        .json(&serde_json::json!({ "name": "platform" }))
        .send()
        .await
        .expect("request to /api/namespaces failed");
    assert_eq!(create.status(), reqwest::StatusCode::OK);

    let body = client
        .post(format!("{base_url}/api/stream/index"))
        .json(&serde_json::json!({
            "path": repo_dir.path().to_string_lossy(),
            "namespace": "platform",
        }))
        .send()
        .await
        .expect("request to /api/stream/index failed")
        .text()
        .await
        .expect("failed to read SSE body");

    assert!(
        body.contains("event: done"),
        "indexing into the new namespace should succeed, got:\n{body}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_stream_rejects_an_invalid_namespace() {
    // The namespace becomes a DuckDB schema name, so it is validated the same
    // way the CLI validates `--namespace` rather than reaching the query layer.
    // Runs on DuckDB storage: the in-memory path rejects any namespace before
    // validation, so this would otherwise pass without exercising the check.
    let repo_dir = tempdir().expect("failed to create repo temp dir");
    std::fs::write(repo_dir.path().join("sample.rs"), "pub fn a() {}\n")
        .expect("failed to write fixture file");

    let (container, _dir) = duckdb_container().await;
    let (base_url, server) = spawn_management_server_with_container(container).await;

    let body = reqwest::Client::new()
        .post(format!("{base_url}/api/stream/index"))
        .json(&serde_json::json!({
            "path": repo_dir.path().to_string_lossy(),
            "namespace": "bad\"name",
        }))
        .send()
        .await
        .expect("request to /api/stream/index failed")
        .text()
        .await
        .expect("failed to read SSE body");

    assert!(
        body.contains("event: error"),
        "an invalid namespace must fail the stream, got:\n{body}"
    );

    server.abort();
}
