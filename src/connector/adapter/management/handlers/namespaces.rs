//! Namespace endpoints.
//!
//! - `POST /api/namespaces` — create a namespace with a fixed embedding config.
//!
//! This mirrors the `codesearch create` CLI command. It exists so that command
//! can be routed through a running `serve` process: `create` only writes
//! namespace configuration (no embedding model is loaded), but it still needs a
//! DuckDB *write* lock, which the running server holds. Rather than fight over
//! the lock, the CLI POSTs here and the server performs the write.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

use crate::{DuckdbVectorRepository, NamespaceEmbeddingConfig};
use crate::{DEFAULT_ONNX_EMBEDDING_MODEL, NO_EMBEDDINGS_MODEL};

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Default embedding dimensionality when the request omits it (all-MiniLM-L6-v2).
const DEFAULT_EMBEDDING_DIMENSIONS: usize = 384;

/// Body of `POST /api/namespaces`. Field meanings match the `create` CLI flags.
#[derive(Debug, Deserialize)]
pub struct CreateNamespaceRequest {
    /// Namespace to create. Required — the server has no per-request "current"
    /// namespace default the way the CLI derives one from the working dir.
    pub name: String,
    /// `"onnx"` (default) or `"api"`.
    #[serde(default)]
    pub embedding_target: Option<String>,
    /// Model identifier; required for `"api"`, defaulted for `"onnx"`.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Vector dimensionality. Defaults to 384.
    #[serde(default)]
    pub embedding_dimensions: Option<usize>,
    /// Create without embeddings — keyword + call-graph search only.
    #[serde(default)]
    pub no_embeddings: bool,
}

/// `POST /api/namespaces` — create a namespace with a fixed embedding config.
///
/// Resolves the same `(target, model, dimensions)` triple the CLI computes,
/// then writes it via the vector repository. Idempotent at the repository
/// level: creating an existing namespace with a matching config is fine; a
/// conflicting embedding config is rejected there and surfaces as an error.
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateNamespaceRequest>,
) -> ApiResult<Json<Value>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }

    let dimensions = req
        .embedding_dimensions
        .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS);
    if dimensions == 0 {
        return Err(ApiError::bad_request(
            "embedding_dimensions must be greater than 0",
        ));
    }

    let (embedding_target, embedding_model) = if req.no_embeddings {
        (
            NO_EMBEDDINGS_MODEL.to_string(),
            NO_EMBEDDINGS_MODEL.to_string(),
        )
    } else {
        match req.embedding_target.as_deref().unwrap_or("onnx") {
            "onnx" => (
                "onnx".to_string(),
                req.embedding_model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ONNX_EMBEDDING_MODEL.to_string()),
            ),
            "api" => {
                let model = req.embedding_model.clone().ok_or_else(|| {
                    ApiError::bad_request("embedding_model is required with embedding_target=api")
                })?;
                ("api".to_string(), model)
            }
            other => {
                return Err(ApiError::bad_request(format!(
                    "unknown embedding_target '{other}' (expected 'onnx' or 'api')"
                )))
            }
        }
    };

    let db_path = Path::new(state.container.data_dir()).join("codesearch.duckdb");
    let cfg = NamespaceEmbeddingConfig {
        embedding_target: embedding_target.clone(),
        embedding_model: embedding_model.clone(),
        dimensions,
    };

    // The DuckDB write happens on a blocking connection; keep it off the async
    // runtime per the project's async rule.
    let name_owned = name.to_string();
    tokio::task::spawn_blocking(move || {
        DuckdbVectorRepository::create_namespace(&db_path, &name_owned, &cfg)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("namespace create task failed: {e}"),
        )
    })??;

    Ok(Json(json!({
        "created": true,
        "namespace": name,
        "embedding_target": embedding_target,
        "embedding_model": embedding_model,
        "embedding_dimensions": dimensions,
    })))
}
