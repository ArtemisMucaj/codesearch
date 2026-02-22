use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::application::QueryExpander;
use crate::domain::DomainError;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const MESSAGES_PATH: &str = "/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-haiku-4-5";
const MAX_TOKENS: u32 = 256;

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

/// Anthropic Messages API request payload.
#[derive(serde::Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(serde::Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Minimal subset of the Anthropic Messages API response we care about.
#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

/// A [`QueryExpander`] that calls the Anthropic Messages API (Claude) to generate
/// semantically rich, code-oriented query variants.
///
/// Falls back gracefully: if the API call fails or returns an unparseable response,
/// the original query is returned as the sole variant rather than propagating an
/// error. This keeps search working even when the API is unavailable.
///
/// **API key**: read from the `ANTHROPIC_API_KEY` environment variable at construction
/// time. If the key is absent the expander returns the original query unchanged.
///
/// **Base URL**: defaults to `https://api.anthropic.com`. Override with
/// `ANTHROPIC_BASE_URL` to target any Anthropic-API-compatible server — e.g.
/// a locally running LM Studio instance (`http://localhost:1234`).
pub struct LlmQueryExpander {
    client: reqwest::Client,
    api_key: String,
    model: String,
    url: String,
}

impl LlmQueryExpander {
    /// Create a new expander with an explicit API key, model, and endpoint URL.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base: String = base_url.into();
        let url = format!("{}{}", base.trim_end_matches('/'), MESSAGES_PATH);
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            url,
        }
    }

    /// Convenience constructor that reads configuration from the environment:
    /// - `ANTHROPIC_API_KEY`  — required; returns `None` when absent
    /// - `ANTHROPIC_BASE_URL` — optional; defaults to `https://api.anthropic.com`
    ///
    /// Set `ANTHROPIC_BASE_URL=http://localhost:1234` to use a locally running
    /// Anthropic-compatible server such as LM Studio with Ministral 3B.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Some(Self::new(key, model, base))
    }

    /// Parse the raw text returned by the model into a `Vec<String>`.
    ///
    /// The model is instructed to return a JSON array; we attempt to parse that.
    /// Any text outside a `[…]` block is ignored to be resilient to minor
    /// formatting deviations.
    fn parse_variants(text: &str) -> Vec<String> {
        // Extract the first JSON array from the response.
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

        let request = ApiRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            system: SYSTEM_PROMPT,
            messages: vec![ApiMessage {
                role: "user",
                content: query,
            }],
        };

        let response = match self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(&request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("LlmQueryExpander: API request failed: {e}. Falling back to original query.");
                return Ok(variants);
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("LlmQueryExpander: API returned {status}: {body}. Falling back to original query.");
            return Ok(variants);
        }

        let api_response: ApiResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                warn!("LlmQueryExpander: failed to deserialize API response: {e}.");
                return Ok(variants);
            }
        };

        if let Some(block) = api_response.content.first() {
            debug!("LlmQueryExpander raw response: {}", block.text);
            let parsed = Self::parse_variants(&block.text);
            variants.extend(parsed);
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
