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
pub mod graph;
pub mod memory;
pub mod repositories;
pub mod search;
