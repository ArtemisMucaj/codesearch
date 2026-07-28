//! Render-ready community graph endpoint.
//!
//! - `GET /api/graph` — the full [`GraphView`] (nodes + edges + communities) for
//!   one repository at the file or symbol level, the same structure the `visualize`
//!   CLI renders to HTML/SVG. Exposes the edge adjacency the `/api/clusters`
//!   endpoints omit, so a client can draw the community graph itself.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::application::{aggregate, DEFAULT_NODE_LIMIT};
use crate::domain::GraphView;

use super::super::error::ApiResult;
use super::super::server::AppState;

/// The graph level to build: file-dependency graph or symbol call graph.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphViewLevel {
    /// File-dependency communities (default).
    #[default]
    File,
    /// Symbol call-graph communities.
    Symbol,
}

/// Query params for `GET /api/graph`.
#[derive(Debug, Deserialize)]
pub struct GraphParams {
    /// Repository to analyse (name or UUID). Omit to auto-detect from the cwd.
    #[serde(default)]
    pub repository: Option<String>,
    /// Graph level: `file` (default) or `symbol`.
    #[serde(default)]
    pub level: GraphViewLevel,
    /// Force aggregation into a per-community meta-graph. When omitted the graph
    /// is aggregated only if it exceeds `DEFAULT_NODE_LIMIT` nodes, matching the
    /// `visualize` CLI so large graphs stay renderable client-side.
    #[serde(default)]
    pub aggregate: Option<bool>,
}

/// `GET /api/graph` — the render-ready [`GraphView`] for one repository at the
/// requested level. Nodes carry community/degree/language; edges reference node
/// indices with a weight and dominant kind; communities carry name/size/cohesion.
pub async fn graph(
    State(state): State<AppState>,
    Query(params): Query<GraphParams>,
) -> ApiResult<Json<GraphView>> {
    let repository_id = state
        .container
        .resolve_repository_id(params.repository.as_deref())
        .await;

    // Build the render-ready graph from the requested level, reusing the same
    // use-case builders the `visualize` CLI drives.
    let view: GraphView = match params.level {
        GraphViewLevel::File => {
            state
                .container
                .cluster_detection_use_case()
                .graph_view(&repository_id)
                .await?
        }
        GraphViewLevel::Symbol => {
            state
                .container
                .symbol_cluster_detection_use_case()
                .graph_view(&repository_id)
                .await?
        }
    };

    // Aggregate to a community meta-graph when explicitly asked, or when the
    // node count would be too large to render node-for-node client-side.
    let should_aggregate = params
        .aggregate
        .unwrap_or_else(|| view.node_count() > DEFAULT_NODE_LIMIT);
    let view = if should_aggregate {
        aggregate(&view)
    } else {
        view
    };

    Ok(Json(view))
}
