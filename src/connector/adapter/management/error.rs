//! Error handling for the management HTTP API.
//!
//! Handlers return [`ApiError`], which converts any `anyhow::Error` (the error
//! type surfaced by every use case) into a consistent JSON body
//! `{"error": "<message>"}` with an appropriate HTTP status code.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::domain::DomainError;

/// A management-API error rendered as `{"error": "..."}` with an HTTP status.
///
/// Handlers use `ApiResult<T>` and `?` to propagate use-case failures; the
/// status is inferred from the underlying [`DomainError`] when present
/// (`NotFound` → 404, everything else → 500).
pub struct ApiError {
    status: StatusCode,
    message: String,
}

/// Convenience alias for handler return types.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// Build an error with an explicit status code.
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// `404 Not Found` with a message.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// `400 Bad Request` with a message.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl ApiError {
    /// Build an `ApiError` from a status and the underlying failure.
    ///
    /// A `NotFound` carries its message to the client (it names the missing
    /// resource and is safe to expose). Any other failure is an internal error:
    /// the full detail is logged server-side and the client receives a generic
    /// message, so binding with `--public` cannot leak paths or backend error
    /// text over the network.
    fn from_status(status: StatusCode, err: impl std::fmt::Display) -> Self {
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("management API internal error: {err}");
            Self::new(status, "internal server error")
        } else {
            Self::new(status, err.to_string())
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        // Map a domain error (possibly nested in the anyhow chain) to its status:
        // NotFound → 404, InvalidInput → 400; anything else is an internal error.
        let status = match err.downcast_ref::<DomainError>() {
            Some(e) if e.is_not_found() => StatusCode::NOT_FOUND,
            Some(e) if e.is_invalid_input() => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::from_status(status, err)
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        let status = if err.is_not_found() {
            StatusCode::NOT_FOUND
        } else if err.is_invalid_input() {
            // Bad client input (e.g. a `0` duration) — a 400, and the message is
            // safe to surface since it names the offending field/constraint.
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self::from_status(status, err)
    }
}
