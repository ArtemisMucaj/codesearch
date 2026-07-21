//! Cluster endpoints — architectural (file-level) and symbol-level communities.
//!
//! - `GET /api/clusters`        — file-dependency Leiden clusters
//! - `GET /api/symbol-clusters` — symbol call-graph communities

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::domain::{ClusterGraph, SymbolCommunityGraph};

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Query params shared by both cluster endpoints.
#[derive(Debug, Deserialize)]
pub struct ClusterParams {
    /// Repository to analyse (name or UUID). Omit to auto-detect from the cwd.
    #[serde(default)]
    pub repository: Option<String>,
    /// Detect clusters across every repository in the namespace instead of a
    /// single one (one global Leiden run, cross-repository edges included;
    /// members are `repo:path`-qualified). File-level clusters only.
    #[serde(default)]
    pub global: bool,
}

impl ClusterParams {
    /// `global=true` cannot be combined with a repository selector.
    fn reject_global_with_repository(&self) -> Result<(), ApiError> {
        if self.global && self.repository.is_some() {
            return Err(ApiError::bad_request(
                "`repository` conflicts with `global`: the namespace-wide graph \
                 spans every repository",
            ));
        }
        Ok(())
    }
}

/// `GET /api/clusters` — architectural clusters over the file dependency graph.
/// Returns the structured [`ClusterGraph`]. With `?global=true`, one Leiden run
/// over every repository in the namespace (cross-repository edges included).
pub async fn clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterParams>,
) -> ApiResult<Json<ClusterGraph>> {
    params.reject_global_with_repository()?;
    let use_case = state.container.cluster_detection_use_case();
    let graph = if params.global {
        // This endpoint has no namespace param, so it uses the server's default.
        use_case.create_namespace_clusters(None).await?
    } else {
        let repository_id = state
            .container
            .resolve_repository_id(params.repository.as_deref())
            .await;
        use_case.create_clusters(&repository_id).await?
    };
    Ok(Json(graph))
}

/// `GET /api/symbol-clusters` — Leiden communities over the symbol call graph.
/// Returns the structured [`SymbolCommunityGraph`].
pub async fn symbol_clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterParams>,
) -> ApiResult<Json<SymbolCommunityGraph>> {
    if params.global {
        return Err(ApiError::bad_request(
            "`global` is not supported for symbol clusters: symbol communities \
             are detected per repository",
        ));
    }
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
