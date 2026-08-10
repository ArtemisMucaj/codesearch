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

use super::super::error::{ApiError, ApiResult};
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
    /// Render the namespace-wide graph instead of one repository's: every
    /// indexed repository, cross-repository edges included, coloured by the
    /// global Leiden clusters. Works at either level.
    #[serde(default)]
    pub global: bool,
    /// Namespace to scope a `global` run to. Defaults to the server's own
    /// namespace. Without this the endpoint silently ignored a client's
    /// requested namespace and built the graph over whichever one `serve` was
    /// started in — so a namespace-wide graph could come back full of another
    /// namespace's repositories while its couplings (which DID honour the
    /// param) described a different set.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Graph level: `file` (default) or `symbol`.
    #[serde(default)]
    pub level: GraphViewLevel,
    /// Force aggregation into a per-community meta-graph. When omitted the graph
    /// is aggregated only if it exceeds `DEFAULT_NODE_LIMIT` nodes, matching the
    /// `visualize` CLI so large graphs stay renderable client-side.
    #[serde(default)]
    pub aggregate: Option<bool>,
}

/// `GET /api/graph` — the render-ready [`GraphView`] for one repository (or,
/// with `?global=true`, the whole namespace) at the requested level. Nodes
/// carry community/degree/language; edges reference node indices with a weight
/// and dominant kind; communities carry name/size/cohesion.
pub async fn graph(
    State(state): State<AppState>,
    Query(params): Query<GraphParams>,
) -> ApiResult<Json<GraphView>> {
    // Communities are detected with only a content-addressed id, and the view
    // materialises each name as it is built — so an unnamed community reaches
    // the client as `c-8c26c91492df`. Cached names are applied by the use cases;
    // any still missing are generated *after* the response, because naming is
    // one LLM call per community and this endpoint already runs Leiden. The next
    // request serves them from cache.
    let view: GraphView = if params.global {
        if params.repository.is_some() {
            return Err(ApiError::bad_request(
                "`repository` conflicts with `global`: the namespace-wide graph \
                 spans every repository",
            ));
        }
        let namespace = params.namespace.as_deref();
        match params.level {
            GraphViewLevel::File => {
                let (view, clusters) = state
                    .container
                    .cluster_detection_use_case()
                    .namespace_graph_view_with_clusters(namespace)
                    .await?;
                super::spawn_cluster_naming(&state, &clusters);
                view
            }
            GraphViewLevel::Symbol => {
                let (view, communities) = state
                    .container
                    .symbol_cluster_detection_use_case()
                    .namespace_graph_view_with_communities(namespace)
                    .await?;
                super::spawn_symbol_naming(&state, &communities);
                view
            }
        }
    } else {
        let repository_id = state
            .container
            .resolve_repository_id(params.repository.as_deref())
            .await;
        match params.level {
            GraphViewLevel::File => {
                let (view, clusters) = state
                    .container
                    .cluster_detection_use_case()
                    .graph_view_with_clusters(&repository_id)
                    .await?;
                super::spawn_cluster_naming(&state, &clusters);
                view
            }
            GraphViewLevel::Symbol => {
                let (view, communities) = state
                    .container
                    .symbol_cluster_detection_use_case()
                    .graph_view_with_communities(&repository_id)
                    .await?;
                super::spawn_symbol_naming(&state, &communities);
                view
            }
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
