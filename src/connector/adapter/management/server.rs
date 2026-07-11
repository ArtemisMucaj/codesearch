//! Management HTTP API server (axum 0.8).
//!
//! Builds an [`axum::Router`] serving a REST/JSON management API and runs it
//! until ctrl-c. The router is assembled by [`routes`], which is the single
//! extension point later PRs hook into.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::handlers;
use crate::connector::api::Container;

use super::streaming::{explain_stream, index_stream};

/// Crate version, surfaced by `/health` and the API index so clients can
/// detect the running server's build.
const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hand-written OpenAPI 3.1 description of the management API, checked in at
/// `docs/management-api.openapi.json` and served verbatim at
/// `GET /api/openapi.json`. Embedded at compile time so the running server is
/// always self-describing without a filesystem dependency.
const OPENAPI_JSON: &str = include_str!("../../../../docs/management-api.openapi.json");

/// Shared state handed to every management route handler.
///
/// Holds the dependency-injection [`Container`] behind an `Arc` so handlers can
/// resolve use cases without rebuilding anything. PR2 (command endpoints) and
/// PR3 (streaming) read the container from here; add new shared dependencies
/// (rate limiters, metrics, etc.) as fields on this struct rather than
/// threading them through `routes` separately.
#[derive(Clone)]
pub struct AppState {
    /// The dependency-injection container wiring adapters to use cases.
    pub container: Arc<Container>,
}

impl AppState {
    /// Build the shared state from an already-constructed container.
    pub fn new(container: Arc<Container>) -> Self {
        Self { container }
    }
}

/// Assemble the management API [`Router`].
///
/// **Extension point.** This is where later PRs attach routes:
/// - PR2 adds per-command REST endpoints (e.g. `POST /api/search`, `GET
///   /api/repositories`) by chaining `.route(...)` calls or merging a
///   sub-router built from `state.container`.
/// - PR3 adds streaming endpoints (SSE / chunked) the same way.
///
/// Keep the signature `fn routes(state: AppState) -> Router` stable so those
/// PRs are additive: new endpoints slot into the builder chain below and share
/// the same [`AppState`]. The state is attached via `.with_state(state)` at the
/// end, so every handler can extract it with `State<AppState>`.
pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api", get(index))
        .route("/health", get(health))
        // ── Per-command REST endpoints ───────────────────────────────────────
        // Repositories + stats.
        .route("/api/repositories", get(handlers::repositories::list))
        .route(
            "/api/repositories/{id}",
            get(handlers::repositories::get).delete(handlers::repositories::delete),
        )
        .route("/api/stats", get(handlers::repositories::stats))
        // Search.
        .route("/api/search", post(handlers::search::search))
        // Call-graph queries.
        .route("/api/impact", post(handlers::graph::impact))
        .route("/api/context/{symbol}", get(handlers::graph::context))
        .route("/api/uses", get(handlers::graph::uses))
        .route("/api/features", get(handlers::graph::features))
        // Clusters / communities.
        .route("/api/clusters", get(handlers::clusters::clusters))
        .route(
            "/api/symbol-clusters",
            get(handlers::clusters::symbol_clusters),
        )
        // Cross-service channels.
        .route("/api/channels", get(handlers::channels::channels))
        // Read-only memory queries.
        .route("/api/memory", get(handlers::memory::list))
        .route("/api/memory/search", get(handlers::memory::search))
        .route("/api/memory/stats", get(handlers::memory::stats))
        .route("/api/memory/sessions", get(handlers::memory::sessions))
        .route("/api/memory/tree", get(handlers::memory::tree))
        .route("/api/memory/{id}", get(handlers::memory::get))
        // ── Streaming (SSE) endpoints ────────────────────────────────────────
        // Live under the `/api/stream/...` prefix so they never clash with the
        // `/api/...` REST routes above.
        .route(
            "/api/stream/explain/{symbol}",
            get(explain_stream).post(explain_stream),
        )
        .route("/api/stream/index", post(index_stream))
        // Machine-readable API description for native-app consumers.
        .route("/api/openapi.json", get(openapi))
        .with_state(state)
}

/// `GET /health` — liveness/readiness probe.
///
/// Returns `200 OK` with `{"status":"ok","version":"<crate version>"}`.
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": API_VERSION,
    }))
}

/// `GET /` and `GET /api` — API index.
///
/// A small self-describing document listing the currently available endpoints.
/// Later PRs should extend the `endpoints` list as they add routes.
async fn index(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": "codesearch-management-api",
        "version": API_VERSION,
        "endpoints": [
            { "method": "GET", "path": "/health", "description": "liveness probe" },
            { "method": "GET", "path": "/api/repositories", "description": "list indexed repositories" },
            { "method": "GET", "path": "/api/repositories/{id}", "description": "one repository + architecture overview" },
            { "method": "DELETE", "path": "/api/repositories/{id}", "description": "delete a repository by ID or path" },
            { "method": "GET", "path": "/api/stats", "description": "index-wide statistics" },
            { "method": "POST", "path": "/api/search", "description": "hybrid semantic + keyword code search" },
            { "method": "POST", "path": "/api/impact", "description": "blast radius of changing a symbol" },
            { "method": "GET", "path": "/api/context/{symbol}", "description": "callers + callees of a symbol" },
            { "method": "GET", "path": "/api/uses", "description": "cross-repo file dependencies (?from=&to=)" },
            { "method": "GET", "path": "/api/features", "description": "entry-point features by criticality" },
            { "method": "GET", "path": "/api/clusters", "description": "file-dependency Leiden clusters" },
            { "method": "GET", "path": "/api/symbol-clusters", "description": "symbol call-graph communities" },
            { "method": "GET", "path": "/api/channels", "description": "cross-service channel links" },
            { "method": "GET", "path": "/api/memory", "description": "list stored memory items (?kind=)" },
            { "method": "GET", "path": "/api/memory/search", "description": "search stored memories (?query=)" },
            { "method": "GET", "path": "/api/memory/stats", "description": "memory item/session counts" },
            { "method": "GET", "path": "/api/memory/sessions", "description": "imported sessions" },
            { "method": "GET", "path": "/api/memory/tree", "description": "browse the memory filesystem (?uri=)" },
            { "method": "GET", "path": "/api/memory/{id}", "description": "one memory item or node" },
            { "method": "GET", "path": "/api/openapi.json", "description": "OpenAPI 3.1 description of this API" },
            { "method": "GET/POST", "path": "/api/stream/explain/{symbol}", "description": "SSE: stream an LLM call-flow explanation for a symbol" },
            { "method": "POST", "path": "/api/stream/index", "description": "SSE: stream indexing progress for a repository path" },
        ],
    }))
}

/// `GET /api/openapi.json` — machine-readable API description.
///
/// Serves the checked-in OpenAPI 3.1 document (embedded at compile time) with a
/// JSON content type so native-app clients can generate typed bindings.
async fn openapi() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON,
    )
}

/// Run the management HTTP API server until ctrl-c.
///
/// Binds to `127.0.0.1:<port>` by default, or `0.0.0.0:<port>` when `public`
/// is set, then serves [`routes`] with graceful shutdown on ctrl-c.
///
/// This intentionally mirrors the MCP HTTP server's lifecycle so both can be
/// driven concurrently from `main` (e.g. via `tokio::select!`).
pub async fn run_management_server(
    container: Arc<Container>,
    port: u16,
    public: bool,
) -> Result<()> {
    let bind_addr: [u8; 4] = if public { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    let addr = SocketAddr::from((bind_addr, port));

    tracing::info!("Starting codesearch management API on {}", addr);

    let state = AppState::new(container);
    let app = routes(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind management API to {addr}"))?;

    tracing::info!("Management API listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down management API");
        })
        .await
        .context("management API server error")?;

    Ok(())
}
