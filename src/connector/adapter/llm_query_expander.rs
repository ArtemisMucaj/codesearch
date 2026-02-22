use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::application::QueryExpander;
use crate::domain::DomainError;

/// Default target: LM Studio running locally on its standard port.
const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const MESSAGES_PATH: &str = "/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Default model matches the LM Studio local-first default.
/// Override with ANTHROPIC_MODEL when targeting the Anthropic cloud.
const DEFAULT_MODEL: &str = "ministral-3b-2512";
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

/// A [`QueryExpander`] that calls any Anthropic-API-compatible server to generate
/// semantically rich, code-oriented query variants.
///
/// **Local-first**: defaults to `http://localhost:1234` (LM Studio) so that no
/// cloud account or API key is needed out of the box. Just load Ministral 3B in
/// LM Studio and pass `--expand-query`.
///
/// To use the Anthropic cloud instead, set:
/// ```text
/// ANTHROPIC_BASE_URL=https://api.anthropic.com
/// ANTHROPIC_API_KEY=sk-ant-...
/// ANTHROPIC_MODEL=claude-haiku-4-5
/// ```
///
/// Falls back gracefully: if the server is unreachable or returns an unparseable
/// response, the original query is returned unchanged so search always succeeds.
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

    /// Construct from environment variables, with local-first defaults:
    ///
    /// | Variable            | Default                    | Purpose                  |
    /// |---------------------|----------------------------|--------------------------|
    /// | `ANTHROPIC_BASE_URL`| `http://localhost:1234`    | LM Studio / any server   |
    /// | `ANTHROPIC_MODEL`   | `ministral-3b-2512`        | Model loaded in LM Studio|
    /// | `ANTHROPIC_API_KEY` | `""` (empty)               | Not required for local   |
    ///
    /// Override all three to target the Anthropic cloud with a real API key.
    pub fn from_env() -> Self {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        Self::new(key, model, base)
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
