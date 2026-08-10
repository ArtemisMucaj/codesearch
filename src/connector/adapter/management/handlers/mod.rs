//! REST/JSON request handlers for the management API.
//!
//! Each handler is a thin adapter: it extracts the shared [`AppState`], reads
//! the request (path/query/body), resolves the matching **use case** from the
//! DI container — the same use cases the CLI drives — and returns structured
//! JSON via `serde`. All business logic stays in the use cases; these functions
//! only translate HTTP ⇄ domain.
//!
//! Handlers return [`ApiResult`], so any use-case error becomes a consistent
//! `{"error": "..."}` body with an appropriate status (see [`super::error`]).

pub mod channels;
pub mod clusters;
pub mod couplings;
pub mod graph;
pub mod graph_view;
pub mod llm;
pub mod namespaces;
pub mod repositories;
pub mod search;

use std::sync::Arc;

use tracing::{debug, warn};

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::LlmUsage;
use crate::connector::api::controller::build_chat_client_for;
use crate::domain::{Cluster, Repository, SymbolCommunity};

use super::error::ApiError;
use super::server::AppState;

/// Resolve a `name-or-UUID` key against an already-fetched repository list,
/// returning the repository's `(id, name)`.
///
/// Matches by exact UUID first, then case-insensitively by name — the lookup
/// every management handler needs. Returns a 404 [`ApiError`] when nothing
/// matches, so callers can simply `?` the result.
fn resolve_repo<'a>(key: &str, repos: &'a [Repository]) -> Result<&'a Repository, ApiError> {
    repos
        .iter()
        .find(|r| r.id() == key)
        .or_else(|| repos.iter().find(|r| r.name().eq_ignore_ascii_case(key)))
        .ok_or_else(|| ApiError::not_found(format!("repository not found: '{key}'")))
}

/// Build the chat client bound to the community-naming usage, or `None` when one
/// can't be constructed (no endpoint configured, malformed config, TLS init
/// failure). Logged at warn: "names are missing" is otherwise indistinguishable
/// from "the LLM had nothing to say".
fn naming_chat_client(state: &AppState) -> Option<Arc<dyn ChatClient>> {
    let target: LlmTarget = state.container.llm_target();
    match build_chat_client_for(
        LlmUsage::LabelCommunities,
        target,
        state.container.data_dir(),
    ) {
        Ok(chat) => Some(chat),
        Err(e) => {
            warn!("community naming disabled, showing ids: {e}");
            None
        }
    }
}

/// Generate any missing LLM display names for `clusters` **in the background**,
/// populating the name cache for subsequent requests.
///
/// Detection produces only a content-addressed id (`c-8c26c91492df`); the CLI
/// names communities inline before printing, but doing that in an HTTP handler
/// would block the response on one LLM call per community — dozens of them on a
/// first view of a real repository, on an endpoint that is already slow because
/// it runs Leiden.
///
/// So the request returns immediately with whatever names are already cached
/// (ids for the rest), and this fills the cache behind it. The next request
/// serves them from cache in milliseconds. A client that wants them without a
/// reload can poll the same endpoint.
///
/// Returns without spawning when nothing is missing, so a warm cache costs one
/// lookup and no task.
///
/// Best-effort by design: names are presentation, not data. An unreachable
/// endpoint or a failed model leaves ids in place and logs; it never affects the
/// response that carries the graph.
fn spawn_cluster_naming(state: &AppState, clusters: &[Cluster]) {
    let missing: Vec<Cluster> = clusters
        .iter()
        .filter(|c| c.display_name.is_none())
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    let Some(chat) = naming_chat_client(state) else {
        return;
    };
    let container = Arc::clone(&state.container);
    tokio::spawn(async move {
        let mut missing = missing;
        debug!(
            "naming {} community/communities in the background",
            missing.len()
        );
        container
            .community_naming_use_case()
            .name_clusters(&mut missing, chat.as_ref())
            .await;
    });
}

/// [`spawn_cluster_naming`] for symbol communities.
fn spawn_symbol_naming(state: &AppState, communities: &[SymbolCommunity]) {
    let missing: Vec<SymbolCommunity> = communities
        .iter()
        .filter(|c| c.display_name.is_none())
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    let Some(chat) = naming_chat_client(state) else {
        return;
    };
    let container = Arc::clone(&state.container);
    tokio::spawn(async move {
        let mut missing = missing;
        debug!(
            "naming {} symbol community/communities in the background",
            missing.len()
        );
        container
            .community_naming_use_case()
            .name_symbol_communities(&mut missing, chat.as_ref())
            .await;
    });
}
