//! Read-only memory query endpoints — long-term memory extracted from finished
//! assistant sessions (mutating memory operations are out of scope for the API).
//!
//! - `GET /api/memory`            — list stored memory items (optional `?kind=`)
//! - `GET /api/memory/search`     — hybrid semantic + keyword search (`?query=`)
//! - `GET /api/memory/stats`      — item / session counts
//! - `GET /api/memory/sessions`   — imported sessions
//! - `GET /api/memory/tree`       — browse the memory virtual filesystem (`?uri=`)
//! - `GET /api/memory/dream`      — dream scheduler status + last recorded run
//! - `POST /api/memory/dream`     — trigger a dream cycle in the background
//! - `GET /api/memory/:id`        — one memory item (ID, `kind/name`, or URI node)

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::application::{MEMORY_ROOT_URI, RESOURCES_ROOT_URI, SESSIONS_ROOT_URI};
use crate::domain::MemoryKind;

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Default number of results for `GET /api/memory/search`.
const DEFAULT_MEMORY_SEARCH_LIMIT: usize = 10;

/// Parse an optional `kind` string into a [`MemoryKind`], rejecting unknowns.
fn parse_kind(kind: Option<&str>) -> ApiResult<Option<MemoryKind>> {
    match kind {
        None => Ok(None),
        Some(k) => MemoryKind::parse(k)
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(format!("unknown memory kind: '{k}'"))),
    }
}

/// Query params for `GET /api/memory` (list).
#[derive(Debug, Deserialize)]
pub struct MemoryListParams {
    /// Restrict to one memory kind (preference, experience, skill, fact).
    #[serde(default)]
    pub kind: Option<String>,
}

/// `GET /api/memory` — list stored memory items, optionally filtered by kind.
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<MemoryListParams>,
) -> ApiResult<Json<Value>> {
    let kind = parse_kind(params.kind.as_deref())?;
    let repo = state.container.memory_repository()?;
    let items = repo.list_items(kind).await?;
    Ok(Json(json!({ "count": items.len(), "items": items })))
}

/// Query params for `GET /api/memory/search`.
#[derive(Debug, Deserialize)]
pub struct MemorySearchParams {
    /// Search query (hybrid semantic + keyword).
    pub query: String,
    /// Maximum number of results.
    #[serde(default = "default_memory_limit")]
    pub num: usize,
    /// Restrict to one memory kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// Restrict to memories relevant in this project/namespace (its items plus
    /// globals). Omit to search every project.
    #[serde(default)]
    pub project: Option<String>,
}

fn default_memory_limit() -> usize {
    DEFAULT_MEMORY_SEARCH_LIMIT
}

/// `GET /api/memory/search?query=...` — hybrid search over stored memories.
/// Each result carries its relevance `score` alongside the item fields.
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<MemorySearchParams>,
) -> ApiResult<Json<Value>> {
    let kind = parse_kind(params.kind.as_deref())?;
    let use_case = state.container.memory_search_use_case()?;
    let results = use_case
        .execute(&params.query, kind, params.project.as_deref(), params.num)
        .await?;

    let items: Vec<Value> = results
        .iter()
        .filter_map(|(item, score)| match serde_json::to_value(item) {
            Ok(mut value) => {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("score".to_string(), json!(score));
                }
                Some(value)
            }
            Err(err) => {
                tracing::warn!("failed to serialize memory item, skipping: {err}");
                None
            }
        })
        .collect();

    Ok(Json(json!({ "count": items.len(), "results": items })))
}

/// `GET /api/memory/stats` — counts of stored items and imported sessions.
pub async fn stats(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let repo = state.container.memory_repository()?;
    let items = repo.list_items(None).await?;
    let sessions = repo.list_sessions().await?;
    Ok(Json(json!({
        "total_items": items.len(),
        "total_sessions": sessions.len(),
    })))
}

/// `GET /api/memory/sessions` — sessions that have been imported into memory.
pub async fn sessions(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let repo = state.container.memory_repository()?;
    let sessions = repo.list_sessions().await?;
    Ok(Json(
        json!({ "count": sessions.len(), "sessions": sessions }),
    ))
}

/// Query params for `GET /api/memory/tree`.
#[derive(Debug, Deserialize)]
pub struct MemoryTreeParams {
    /// Directory URI to list (e.g. `memory://sessions`). Omit for the root view.
    #[serde(default)]
    pub uri: Option<String>,
}

/// `GET /api/memory/tree` — browse the memory virtual filesystem. With no
/// `uri`, returns the digest node plus the sessions/resources directories.
pub async fn tree(
    State(state): State<AppState>,
    Query(params): Query<MemoryTreeParams>,
) -> ApiResult<Json<Value>> {
    let repo = state.container.memory_repository()?;
    let children = match params.uri.as_deref() {
        None => {
            let mut nodes = Vec::new();
            if let Some(digest) = repo.find_node(MEMORY_ROOT_URI).await? {
                nodes.push(digest);
            }
            nodes.extend(repo.list_child_nodes(SESSIONS_ROOT_URI).await?);
            nodes.extend(repo.list_child_nodes(RESOURCES_ROOT_URI).await?);
            nodes
        }
        Some(dir) => repo.list_child_nodes(dir).await?,
    };
    Ok(Json(json!({ "count": children.len(), "nodes": children })))
}

/// `GET /api/memory/dream` — scheduler configuration, whether a cycle is in
/// flight, and the last recorded run.
pub async fn dream_status(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let Some(dream) = state.dream.as_ref() else {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "dreaming is not available on this server (no LLM backend configured at startup)",
        ));
    };
    let config = dream.config();
    Ok(Json(json!({
        "enabled": config.dream_enabled(),
        "interval_hours": config.dream_interval_hours(),
        "session_idle_minutes": config.session_idle_minutes(),
        "auto_import": config.auto_import(),
        "running": dream.is_running(),
        "last_run": dream.last_run().await,
    })))
}

/// `PUT /api/memory/dream/config` — update the dream scheduler's settings.
///
/// Accepts a partial body (`dream_enabled`, `dream_interval_hours`,
/// `session_idle_minutes`, `auto_import`); omitted fields are left unchanged.
/// The change is persisted to `config.json` and applied to the running
/// scheduler live (it reads a fresh snapshot each tick), so no restart is
/// needed. Returns the merged, effective config — the same shape as
/// `GET /api/memory/dream`'s configuration fields.
pub async fn dream_config(
    State(state): State<AppState>,
    Json(patch): Json<super::super::MemoryConfigPatch>,
) -> ApiResult<Json<Value>> {
    let Some(dream) = state.dream.as_ref() else {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "dreaming is not available on this server (no LLM backend configured at startup)",
        ));
    };
    let config = dream.update_config(patch).await?;
    Ok(Json(json!({
        "dream_enabled": config.dream_enabled(),
        "dream_interval_hours": config.dream_interval_hours(),
        "session_idle_minutes": config.session_idle_minutes(),
        "auto_import": config.auto_import(),
    })))
}

/// `POST /api/memory/dream` — start a dream cycle in the background. Returns
/// `202` immediately; progress lands in the server log and the run record is
/// readable via `GET /api/memory/dream` once finished.
pub async fn dream_trigger(
    State(state): State<AppState>,
) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
    let Some(dream) = state.dream.as_ref() else {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "dreaming is not available on this server (no LLM backend configured at startup)",
        ));
    };
    if !dream.trigger() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a dream cycle is already running",
        ));
    }
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({ "started": true })),
    ))
}

/// `GET /api/memory/:id` — resolve one memory item or virtual-filesystem node.
///
/// `:id` accepts a memory item UUID, a `kind/name` reference, or a
/// `memory://…` node URI (matching the CLI `memory show`).
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let repo = state.container.memory_repository()?;

    // `memory://` addresses a virtual-filesystem node rather than a flat item.
    if id.starts_with("memory://") {
        return match repo.find_node(&id).await? {
            Some(node) => Ok(Json(json!({ "node": node }))),
            None => Err(ApiError::not_found(format!("no memory node at '{id}'"))),
        };
    }

    // Accept `<kind>/<name>` as an alternative to the item ID. A valid kind is
    // an unambiguous reference, so report against it rather than falling through
    // to the ID lookup (which would give a misleading "no item with ID" error).
    if let Some((kind_str, name)) = id.split_once('/') {
        if let Some(kind) = MemoryKind::parse(kind_str) {
            return match repo.find_item(kind, name).await? {
                Some(item) => Ok(Json(json!({ "item": item }))),
                None => Err(ApiError::not_found(format!(
                    "no memory item '{name}' of kind '{kind_str}'"
                ))),
            };
        }
    }

    match repo.find_item_by_id(&id).await? {
        Some(item) => Ok(Json(json!({ "item": item }))),
        None => Err(ApiError::not_found(format!(
            "no memory item with ID '{id}'"
        ))),
    }
}
