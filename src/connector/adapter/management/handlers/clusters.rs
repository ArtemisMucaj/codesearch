//! Cluster endpoints — architectural (file-level) and symbol-level communities.
//!
//! - `GET /api/clusters`        — file-dependency Leiden clusters
//! - `GET /api/symbol-clusters` — symbol call-graph communities

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::domain::{ClusterGraph, SymbolCommunityGraph};

use super::super::error::ApiResult;
use super::super::server::AppState;

/// Query params shared by both cluster endpoints.
#[derive(Debug, Deserialize)]
pub struct ClusterParams {
    /// Repository to analyse (name or UUID). Omit to auto-detect from the cwd.
    #[serde(default)]
    pub repository: Option<String>,
}

/// `GET /api/clusters` — architectural clusters over the file dependency graph.
/// Returns the structured [`ClusterGraph`].
pub async fn clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterParams>,
) -> ApiResult<Json<ClusterGraph>> {
    let repository_id = state
        .container
        .resolve_repository_id(params.repository.as_deref())
        .await;
    let graph = state
        .container
        .cluster_detection_use_case()
        .create_clusters(&repository_id)
        .await?;
    Ok(Json(graph))
}

/// `GET /api/symbol-clusters` — Leiden communities over the symbol call graph.
/// Returns the structured [`SymbolCommunityGraph`].
pub async fn symbol_clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterParams>,
) -> ApiResult<Json<SymbolCommunityGraph>> {
    let repository_id = state
        .container
        .resolve_repository_id(params.repository.as_deref())
        .await;
    let graph = state
        .container
        .symbol_cluster_detection_use_case()
        .detect_communities(&repository_id)
        .await?;
    Ok(Json(graph))
}
