//! Error mapping for the LLM crates at the connector boundary.
//!
//! `openai-rs` and `gh-copilot-rs` are connector-layer concerns, so their error
//! types are translated into [`DomainError`] here rather than through `From`
//! impls in `src/domain/` — the domain layer stays free of external crates
//! beyond `serde`, keeping dependencies pointing inward.
//!
//! Both mappers walk the [`std::error::Error`] source chain, because
//! `to_string()` on these errors renders only the outermost message and drops
//! the underlying transport/parse cause that makes a failure diagnosable.

use std::error::Error;

use crate::domain::DomainError;

/// Render `error` plus its full source chain as `outer: cause: root`.
fn chain(error: &dyn Error) -> String {
    let mut msg = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        // Crates often embed the cause in the outer Display already; skip the
        // duplicate rather than repeat it.
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = cause.source();
    }
    msg
}

/// Map an [`openai_rs::OpenAiError`] into a [`DomainError`].
pub fn map_openai_err(error: openai_rs::OpenAiError) -> DomainError {
    DomainError::internal(chain(&error))
}

/// Map a [`gh_copilot_rs::CopilotError`] into a [`DomainError`].
pub fn map_copilot_err(error: gh_copilot_rs::CopilotError) -> DomainError {
    DomainError::internal(chain(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_openai_error_to_an_internal_domain_error() {
        let err = map_openai_err(openai_rs::OpenAiError::configuration("bad base url"));
        assert!(matches!(err, DomainError::Internal(_)));
        assert!(err.to_string().contains("bad base url"));
    }

    #[test]
    fn maps_copilot_error_to_an_internal_domain_error() {
        let err = map_copilot_err(gh_copilot_rs::CopilotError::configuration("no token"));
        assert!(matches!(err, DomainError::Internal(_)));
        assert!(err.to_string().contains("no token"));
    }

    #[test]
    fn chain_appends_a_distinct_source_without_duplicating_it() {
        #[derive(Debug)]
        struct Root;
        impl std::fmt::Display for Root {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection refused")
            }
        }
        impl Error for Root {}

        #[derive(Debug)]
        struct Outer(Root);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "request failed")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(chain(&Outer(Root)), "request failed: connection refused");
    }
}
