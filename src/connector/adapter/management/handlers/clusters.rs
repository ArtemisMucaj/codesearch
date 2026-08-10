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
    /// Namespace to scope a `global` run to. Defaults to the server's own
    /// namespace. Without this the endpoint silently ignored a client's
    /// requested namespace and analysed whichever one `serve` was started in,
    /// so a namespace-wide graph could come back full of another namespace's
    /// repositories.
    #[serde(default)]
    pub namespace: Option<String>,
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
        use_case
            .create_namespace_clusters(params.namespace.as_deref())
            .await?
    } else {
        let repository_id = state
            .container
            .resolve_repository_id(params.repository.as_deref())
            .await;
        use_case.create_clusters(&repository_id).await?
    };
    // Detection yields content-addressed ids; cached display names are already
    // applied by the use case. Generate any still missing in the background so
    // the next request has them, rather than blocking this one on an LLM call
    // per unnamed community.
    super::spawn_cluster_naming(&state, &graph.clusters);
    Ok(Json(graph))
}

/// `GET /api/symbol-clusters` — Leiden communities over the symbol call graph.
/// Returns the structured [`SymbolCommunityGraph`].
pub async fn symbol_clusters(
    State(state): State<AppState>,
    Query(params): Query<ClusterParams>,
) -> ApiResult<Json<SymbolCommunityGraph>> {
    params.reject_global_with_repository()?;
    let use_case = state.container.symbol_cluster_detection_use_case();
    let graph = if params.global {
        // One Leiden run over every repository's call graph in the namespace.
        // This used to 400 ("symbol communities are detected per repository"),
        // which left the symbol level with no namespace-wide view at all.
        use_case
            .create_namespace_symbol_communities(params.namespace.as_deref())
            .await?
    } else {
        let repository_id = state
            .container
            .resolve_repository_id(params.repository.as_deref())
            .await;
        use_case.detect_communities(&repository_id).await?
    };
    super::spawn_symbol_naming(&state, &graph.communities);
    Ok(Json(graph))
}
