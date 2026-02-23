use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::application::QueryExpander;
use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

/// System prompt instructing the model to produce terse, code-oriented query variants.
const SYSTEM_PROMPT: &str = "\
You are a code search assistant. Your task is to expand a user's natural language \
code search query into alternative phrasings that help retrieve relevant code \
using semantic embeddings.

Rules:
1. Return ONLY a JSON array of strings — no prose, no markdown, no code fences.
2. Generate exactly 2 alternative phrasings of the query (do not include the original).
3. Each alternative must be concise (≤ 10 words).
4. Focus on technical terms: function/method names, data structures, patterns, identifiers.
5. One variant should be a terse technical description; the other should look like \
   snake_case or camelCase identifiers a developer would actually name their code.

Example input:  \"find the function that handles user authentication errors\"
Example output: [\"authentication error handler\", \"handle_auth_error user_login_failure\"]";

/// A [`QueryExpander`] that delegates to a [`ChatClient`] to generate
/// semantically rich, code-oriented query variants.
///
/// All transport, serialization, and API-vendor details are handled by the
/// injected client.  This struct only knows the [`ChatClient`] interface and
/// the prompt engineering needed for code search.
///
/// Falls back gracefully to the original query when the client returns an error
/// or an unparseable response, so search always succeeds even if the LLM is
/// unavailable.
pub struct LlmQueryExpander {
    client: Arc<dyn ChatClient>,
}

impl LlmQueryExpander {
    pub fn new(client: Arc<dyn ChatClient>) -> Self {
        Self { client }
    }

    /// Parse the raw text returned by the model into a `Vec<String>`.
    ///
    /// The model is instructed to return a JSON array; we attempt to parse that.
    /// Any text outside a `[…]` block is ignored to be resilient to minor
    /// formatting deviations.
    fn parse_variants(text: &str) -> Vec<String> {
        let start = text.find('[');
        let end = text.rfind(']');

        if let (Some(s), Some(e)) = (start, end) {
            if let Ok(variants) = serde_json::from_str::<Vec<String>>(&text[s..=e]) {
                return variants
                    .into_iter()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect();
            }
        }

        warn!("LlmQueryExpander: could not parse model response as JSON array: {text}");
        vec![]
    }
}

#[async_trait]
impl QueryExpander for LlmQueryExpander {
    async fn expand(&self, query: &str) -> Result<Vec<String>, DomainError> {
        let mut variants = vec![query.to_string()];

        match self.client.complete(SYSTEM_PROMPT, query).await {
            Ok(text) => {
                debug!("LlmQueryExpander raw response: {text}");
                variants.extend(Self::parse_variants(&text));
            }
            Err(e) => {
                warn!("LlmQueryExpander: client error: {e}. Falling back to original query.");
            }
        }

        Ok(variants)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants_extracts_json_array() {
        let text = r#"["authentication error handler", "handle_auth_error"]"#;
        let variants = LlmQueryExpander::parse_variants(text);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0], "authentication error handler");
        assert_eq!(variants[1], "handle_auth_error");
    }

    #[test]
    fn parse_variants_tolerates_surrounding_prose() {
        let text = r#"Here are your variants: ["fetch user data", "get_user_profile"] done."#;
        let variants = LlmQueryExpander::parse_variants(text);
        assert_eq!(variants.len(), 2);
    }

    #[test]
    fn parse_variants_returns_empty_on_invalid_json() {
        let variants = LlmQueryExpander::parse_variants("not json at all");
        assert!(variants.is_empty());
    }

    #[test]
    fn parse_variants_filters_empty_strings() {
        let text = r#"["valid", "", "  ", "also valid"]"#;
        let variants = LlmQueryExpander::parse_variants(text);
        assert_eq!(variants.len(), 2);
    }
}
