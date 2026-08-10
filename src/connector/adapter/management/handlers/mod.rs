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

use tracing::warn;

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::LlmUsage;
use crate::connector::api::controller::build_chat_client_for;
use crate::domain::Repository;

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

/// Fill in LLM display names for detected communities, exactly as the CLI does.
///
/// Detection only produces a content-addressed id (`c-8c26c91492df`), so without
/// this every community reaches an API client as that id. The CLI's `clusters`,
/// `symbol-clusters` and `overview` commands all name them before printing; the
/// management API did not, so a GUI driving this API could never show a readable
/// name no matter how its LLM was configured.
///
/// **Best-effort by design.** Names are a presentation nicety, not data: a
/// missing endpoint, an unreachable server, or a model that fails mid-run must
/// degrade to ids rather than fail the request that carries the graph. Every
/// failure path here logs and returns.
///
/// Cheap after the first call — [`CommunityNamingUseCase`] caches by stable id,
/// so a repeated request pays a cache read, and a re-index only re-names the
/// communities whose membership actually changed.
///
/// [`CommunityNamingUseCase`]: crate::application::CommunityNamingUseCase
/// Build the chat client bound to the community-naming usage, or `None` when one
/// can't be constructed (no endpoint configured, malformed config, TLS init
/// failure). Logged at warn: "names are missing" is otherwise indistinguishable
/// from "the LLM had nothing to say".
///
/// The caller pairs this with the naming use case to make a
/// [`ClusterNamer`](crate::application::ClusterNamer).
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
