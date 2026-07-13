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
pub mod memory;
pub mod repositories;
pub mod search;

use crate::domain::Repository;

use super::error::ApiError;

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
