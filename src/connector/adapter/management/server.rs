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
        // PR2: per-command REST endpoints attach here.
        // PR3 (streaming): SSE endpoints live under the `/api/stream/...`
        // prefix so they never clash with PR2's `/api/...` REST routes.
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
