//! Call-graph endpoints: impact (blast radius), context (callers/callees),
//! cross-repo `uses`, and execution features.
//!
//! - `POST /api/impact`          — blast radius of changing a symbol
//! - `GET  /api/context/:symbol` — callers + callees tree for a symbol
//! - `GET  /api/uses`            — files in `from` that reference symbols in `to`
//! - `GET  /api/features`        — entry-point features ranked by criticality

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

use crate::domain::Repository;

use super::super::error::ApiResult;
use super::super::server::AppState;

/// Default number of features returned by `GET /api/features`.
const DEFAULT_FEATURE_LIMIT: usize = 20;

/// Body for `POST /api/impact`.
#[derive(Debug, Deserialize)]
pub struct ImpactRequest {
    /// Symbol name or regex pattern.
    pub symbol: String,
    /// Restrict analysis to a specific repository (name or UUID).
    #[serde(default)]
    pub repository: Option<String>,
    /// Treat `symbol` as a literal regex instead of auto-wrapping `.*symbol.*`.
    #[serde(default)]
    pub regex: bool,
}

/// `POST /api/impact` — who is affected if this symbol changes (BFS up the call
/// graph). Returns the structured [`crate::ImpactAnalysis`].
pub async fn impact(
    State(state): State<AppState>,
    Json(req): Json<ImpactRequest>,
) -> ApiResult<Json<crate::ImpactAnalysis>> {
    let analysis = state
        .container
        .impact_use_case()
        .analyze(&req.symbol, req.repository.as_deref(), req.regex)
        .await?;
    Ok(Json(analysis))
}

/// Query params shared by the symbol-context endpoint.
#[derive(Debug, Deserialize)]
pub struct ContextParams {
    /// Restrict context to a specific repository (name or UUID).
    #[serde(default)]
    pub repository: Option<String>,
    /// Treat the symbol as a literal regex.
    #[serde(default)]
    pub regex: bool,
}

/// `GET /api/context/:symbol` — callers (entry points → symbol) and callees
/// (symbol → leaves). Returns the structured [`crate::SymbolContext`].
pub async fn context(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    Query(params): Query<ContextParams>,
) -> ApiResult<Json<crate::SymbolContext>> {
    let ctx = state
        .container
        .context_use_case()
        .get_context(&symbol, params.repository.as_deref(), params.regex)
        .await?;
    Ok(Json(ctx))
}

/// Query params for the cross-repo `uses` endpoint.
#[derive(Debug, Deserialize)]
pub struct UsesParams {
    /// Repository doing the using (caller side) — name or UUID.
    pub from: String,
    /// Repository being used (dependency side) — name or UUID.
    pub to: String,
}

/// `GET /api/uses?from=<repo>&to=<repo>` — files in `from` that reference
/// symbols defined in `to`. Returns the filtered cross-repo edges.
pub async fn uses(
    State(state): State<AppState>,
    Query(params): Query<UsesParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let all_repos: Vec<Repository> = state.container.list_use_case().execute().await?;

    let from = super::resolve_repo(&params.from, &all_repos)?;
    let (from_id, from_name) = (from.id().to_string(), from.name().to_string());
    let to = super::resolve_repo(&params.to, &all_repos)?;
    let (to_id, to_name) = (to.id().to_string(), to.name().to_string());

    let graph = state
        .container
        .file_graph_use_case()
        .build_graph(Some(&[from_id.clone(), to_id.clone()]), 1, true)
        .await?;

    let edges: Vec<&crate::domain::FileEdge> = graph
        .edges
        .iter()
        .filter(|e| e.from_repo_id == from_id && e.to_repo_id == to_id)
        .collect();

    Ok(Json(serde_json::json!({
        "from": { "id": from_id, "name": from_name },
        "to": { "id": to_id, "name": to_name },
        "edges": edges,
    })))
}

/// Query params for `GET /api/features`.
#[derive(Debug, Deserialize)]
pub struct FeaturesParams {
    /// Restrict to a specific repository (name or UUID). Omit to auto-detect.
    #[serde(default)]
    pub repository: Option<String>,
    /// Maximum number of features to return.
    #[serde(default = "default_feature_limit")]
    pub limit: usize,
}

fn default_feature_limit() -> usize {
    DEFAULT_FEATURE_LIMIT
}

/// `GET /api/features` — entry-point execution features sorted by criticality.
pub async fn features(
    State(state): State<AppState>,
    Query(params): Query<FeaturesParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let repository_id = state
        .container
        .resolve_repository_id(params.repository.as_deref())
        .await;
    let features = state
        .container
        .execution_features_use_case()
        .list_features(&repository_id, params.limit)
        .await?;

    Ok(Json(serde_json::json!({
        "repository_id": repository_id,
        "count": features.len(),
        "features": features,
    })))
}
