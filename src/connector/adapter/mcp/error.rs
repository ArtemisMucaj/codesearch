//! Error handling and result serialization for the MCP server.
//!
//! Mirrors [`super::super::management::error`]: one conversion decides how a
//! use-case failure is reported to the client, instead of every tool hand-mapping
//! its own. That was both bulk — 43 near-identical closures across 16 tools — and
//! a source of drift: the same missing repository came back as an internal error
//! over MCP and as a 404 over HTTP.
//!
//! rmcp requires `#[tool]` methods to fail with its own [`McpError`], and the
//! orphan rule forbids `impl From<anyhow::Error> for McpError` (both types are
//! foreign). So the conversion is a plain function, [`tool_error`], applied with
//! `.map_err(tool_error)?`.

use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData as McpError;
use serde::Serialize;

use crate::domain::DomainError;

/// Map a use-case failure onto the MCP error taxonomy.
///
/// A `NotFound` or `InvalidInput` is the caller's mistake — a symbol that isn't
/// indexed, an unknown repository, a malformed level — so it comes back as
/// `invalid_params` carrying its message: the client can act on it, and the text
/// names only what the caller itself supplied. Anything else is ours: the detail
/// is logged server-side and the client gets a generic message, matching the
/// management API's reasoning about not leaking backend paths or error text.
pub fn tool_error(err: impl Into<anyhow::Error>) -> McpError {
    let err: anyhow::Error = err.into();
    match err.downcast_ref::<DomainError>() {
        Some(e) if e.is_not_found() || e.is_invalid_input() => {
            // `{err:#}` renders the whole chain; plain Display would report only
            // the outermost `.context(..)`, dropping the symbol that failed.
            McpError::invalid_params(format!("{err:#}"), None)
        }
        _ => {
            tracing::error!("MCP tool internal error: {err:#}");
            McpError::internal_error("internal server error", None)
        }
    }
}

/// Serialize `value` as the pretty JSON body of a successful tool call.
///
/// Every tool ended with this same three-step ritual. Serialization of a plain
/// `Serialize` type effectively cannot fail here, but it is reported rather than
/// unwrapped.
pub fn ok_json<T: Serialize>(value: T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(&value).map_err(|e| {
        tracing::error!("MCP tool failed to serialize its result: {e}");
        McpError::internal_error("failed to serialize result", None)
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A not-found is the caller's problem and must carry its message, so an
    /// agent can correct the symbol or repository it asked for.
    #[test]
    fn not_found_becomes_invalid_params_and_keeps_its_message() {
        let mcp = tool_error(DomainError::not_found("repository not found: 'nope'"));

        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            mcp.message.contains("repository not found"),
            "caller-facing detail must survive, got: {}",
            mcp.message
        );
    }

    /// The inverse: an internal failure must NOT leak backend detail to the
    /// client — the same guarantee the management API makes when bound public.
    #[test]
    fn internal_errors_are_redacted() {
        let mcp = tool_error(anyhow::anyhow!(
            "duckdb: /Users/someone/.codesearch/db is locked"
        ));

        assert_eq!(mcp.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            !mcp.message.contains("/Users/someone"),
            "internal detail must not reach the client, got: {}",
            mcp.message
        );
    }

    /// A not-found nested in an anyhow context chain still classifies, and the
    /// detail underneath the context survives.
    #[test]
    fn a_wrapped_not_found_is_still_classified() {
        let mcp = tool_error(
            anyhow::Error::new(DomainError::not_found("symbol 'x'"))
                .context("resolving the impact root"),
        );

        assert_eq!(mcp.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            mcp.message.contains("symbol 'x'"),
            "the wrapped caller-facing detail must survive, got: {}",
            mcp.message
        );
    }

    #[test]
    fn ok_json_wraps_the_value_as_pretty_text_content() {
        let result =
            ok_json(serde_json::json!({ "total": 2 })).expect("serializing a plain value succeeds");

        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("tool results carry a single text block");
        assert!(text.contains("\"total\": 2"), "got: {text}");
    }
}
