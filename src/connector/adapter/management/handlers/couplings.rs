//! Coupling endpoint — the glue holding internally-fragile Leiden communities
//! together.
//!
//! - `GET /api/couplings` — files/symbols or dependencies whose removal would
//!   split a community into two latent sub-blocks (the hub-like dependency /
//!   modularity-violation smell), at the file or symbol level.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::domain::{CouplingReport, GraphLevel};

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Query params for the couplings endpoint.
#[derive(Debug, Deserialize)]
pub struct CouplingParams {
    /// Repository to analyse (name or UUID). Omit to auto-detect from the cwd.
    /// Ignored when `global` is set.
    #[serde(default)]
    pub repository: Option<String>,

    /// Which graph to analyse: `file` (default) or `symbol`.
    #[serde(default)]
    pub level: Option<String>,

    /// Namespace-wide couplings: run the pipeline over every repository in the
    /// namespace (cross-repository edges included). A coupler that splits a
    /// namespace-wide community is the shared element welding two repositories
    /// together. Works at both levels.
    #[serde(default)]
    pub global: bool,

    /// Namespace to analyse for a `global` run. Omit to use the server's default.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `GET /api/couplings` — coupling elements in the repository's Leiden
/// communities. Returns the structured [`CouplingReport`].
pub async fn couplings(
    State(state): State<AppState>,
    Query(params): Query<CouplingParams>,
) -> ApiResult<Json<CouplingReport>> {
    let level = match params.level.as_deref() {
        None => GraphLevel::File,
        Some(s) => GraphLevel::parse(s)
            .map_err(|msg| ApiError::bad_request(format!("invalid level '{s}': {msg}")))?,
    };

    let report = if params.global {
        let namespace = params
            .namespace
            .as_deref()
            .unwrap_or_else(|| state.container.namespace());
        state
            .container
            .coupling_detection_use_case()
            .detect_namespace(namespace, level)
            .await?
    } else {
        let repository_id = state
            .container
            .resolve_repository_id(params.repository.as_deref())
            .await;
        state
            .container
            .coupling_detection_use_case()
            .detect(&repository_id, level)
            .await?
    };
    Ok(Json(report))
}
