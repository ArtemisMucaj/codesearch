//! Repository + stats endpoints.
//!
//! - `GET  /api/repositories`      — list indexed repositories
//! - `GET  /api/repositories/:id`  — one repository + its cluster architecture overview
//! - `DELETE /api/repositories/:id`— delete a repository by ID or path
//! - `GET  /api/stats`             — index-wide statistics

use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

use crate::domain::{DomainError, Repository};

use super::super::error::ApiResult;
use super::super::server::AppState;

/// Serialize a [`Repository`] into a stable JSON object for API responses.
///
/// The domain type already derives `Serialize`, but its private fields don't
/// map cleanly to the accessor-based shape the CLI prints; building the object
/// explicitly keeps the wire format decoupled from internal representation.
fn repository_json(repo: &Repository) -> Value {
    let languages: Value = repo
        .languages()
        .iter()
        .map(|(lang, stats)| (lang.clone(), json!({ "file_count": stats.file_count })))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    json!({
        "id": repo.id(),
        "name": repo.name(),
        "path": repo.path(),
        "file_count": repo.file_count(),
        "chunk_count": repo.chunk_count(),
        "store": repo.store().as_str(),
        "namespace": repo.namespace(),
        "languages": languages,
    })
}

/// `GET /api/repositories` — list every repository indexed in the namespace.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let repos = state.container.list_use_case().execute().await?;
    let items: Vec<Value> = repos.iter().map(repository_json).collect();
    Ok(Json(json!({ "repositories": items })))
}

/// `GET /api/repositories/:id` — a single repository plus its architecture
/// overview (the Markdown cluster summary from the cluster-detection use case).
///
/// `:id` accepts a repository name or UUID (resolved the same way the CLI
/// resolves `--repository`).
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let repos = state.container.list_use_case().execute().await?;
    let repo = super::resolve_repo(&id, &repos)?;

    // Architecture overview is best-effort: a repository with no call graph
    // (never SCIP-indexed) has no clusters, which is not an error.
    let overview = state
        .container
        .cluster_detection_use_case()
        .architecture_overview(repo.id())
        .await
        .unwrap_or_default();

    let mut body = repository_json(repo);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("architecture_overview".to_string(), json!(overview));
    }
    Ok(Json(body))
}

/// `DELETE /api/repositories/:id` — delete a repository by ID, falling back to
/// deletion by path when the ID is not found (mirrors the CLI `delete`).
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let use_case = state.container.delete_use_case();
    match use_case.execute(&id).await {
        Ok(_) => Ok(Json(json!({ "deleted": true, "id": id }))),
        Err(DomainError::NotFound(_)) => {
            // Retry as a path; if that also fails the error propagates as 404/500.
            use_case.delete_by_path(&id).await?;
            Ok(Json(json!({ "deleted": true, "id": id })))
        }
        Err(e) => Err(e.into()),
    }
}

/// `GET /api/stats` — aggregate index statistics across all repositories.
pub async fn stats(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let repos = state.container.list_use_case().execute().await?;
    let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
    let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

    Ok(Json(json!({
        "repositories": repos.len(),
        "total_files": total_files,
        "total_chunks": total_chunks,
        "data_dir": state.container.data_dir(),
        "namespace": state.container.namespace(),
    })))
}
