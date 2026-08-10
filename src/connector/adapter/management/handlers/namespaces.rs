//! Namespace endpoints.
//!
//! - `GET    /api/namespaces`        — list every configured namespace.
//! - `POST   /api/namespaces`        — create a namespace with a fixed embedding config.
//! - `DELETE /api/namespaces/{name}` — delete a namespace and everything in it.
//!
//! `POST` mirrors the `codesearch create` CLI command. It exists so that command
//! can be routed through a running `serve` process: `create` only writes
//! namespace configuration (no embedding model is loaded), but it still needs a
//! DuckDB *write* lock, which the running server holds. Rather than fight over
//! the lock, the CLI POSTs here and the server performs the write. `GET` and
//! `DELETE` need that same write lock (or a consistent read of it), so they are
//! served here for the same reason.

use axum::extract::{Path as AxumPath, State};
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

/// `GET /api/namespaces` — every configured namespace with its embedding config
/// and the repositories indexed into it.
///
/// Read from `namespace_config`, not derived from the `repositories` table, so
/// a namespace that was created but never indexed into is still listed. A
/// client that groups repositories by their `namespace` field cannot see those,
/// which is precisely the gap this endpoint fills.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db_path = Path::new(state.container.data_dir()).join("codesearch.duckdb");

    let namespaces =
        tokio::task::spawn_blocking(move || DuckdbVectorRepository::list_namespaces(&db_path))
            .await
            .map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("namespace list task failed: {e}"),
                )
            })??;

    // Repository counts come from the metadata store, so one call answers
    // "which namespaces exist" and "what is in them" together.
    let repos = state.container.list_use_case().execute().await?;

    let items: Vec<Value> = namespaces
        .iter()
        .map(|ns| {
            let members: Vec<&crate::domain::Repository> = repos
                .iter()
                .filter(|r| r.namespace() == Some(ns.name.as_str()))
                .collect();
            json!({
                "name": ns.name,
                "embedding_target": ns.embedding_target,
                "embedding_model": ns.embedding_model,
                "embedding_dimensions": ns.dimensions,
                "repositories": members.len(),
                "total_files": members.iter().map(|r| r.file_count()).sum::<u64>(),
                "total_chunks": members.iter().map(|r| r.chunk_count()).sum::<u64>(),
            })
        })
        .collect();

    Ok(Json(json!({ "namespaces": items })))
}

/// `DELETE /api/namespaces/{name}` — delete a namespace and everything indexed
/// into it.
///
/// Cascading by design: every repository in the namespace is deleted through
/// `DeleteRepositoryUseCase` first — the one place that knows every global
/// table a repository touches (chunks, embeddings, call graph, file hashes,
/// channel endpoints, cached analyses) — and only then is the namespace's own
/// schema and config row dropped. Doing it in that order means a failure
/// part-way leaves the namespace present and re-deletable, rather than orphaning
/// repository rows behind a config that no longer exists.
pub async fn delete(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }

    // Delete the member repositories first, so nothing is left keyed to a
    // namespace that no longer has a config row.
    let repos = state.container.list_use_case().execute().await?;
    let members: Vec<String> = repos
        .iter()
        .filter(|r| r.namespace() == Some(name.as_str()))
        .map(|r| r.id().to_string())
        .collect();

    let delete_use_case = state.container.delete_use_case();
    for id in &members {
        delete_use_case.execute(id).await?;
    }

    let db_path = Path::new(state.container.data_dir()).join("codesearch.duckdb");
    let name_owned = name.clone();
    let existed = tokio::task::spawn_blocking(move || {
        DuckdbVectorRepository::drop_namespace(&db_path, &name_owned)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("namespace delete task failed: {e}"),
        )
    })??;

    if !existed && members.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("Namespace '{name}' not found"),
        ));
    }

    Ok(Json(json!({
        "deleted": true,
        "namespace": name,
        "repositories_deleted": members.len(),
    })))
}
