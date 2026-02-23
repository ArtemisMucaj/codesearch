use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

/// Default target: LM Studio running locally on its standard port.
pub const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const MESSAGES_PATH: &str = "/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Default model matches the LM Studio local-first default.
const DEFAULT_MODEL: &str = "ministral-3b-2512";
const MAX_TOKENS: u32 = 256;

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

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

/// HTTP client for the Anthropic Messages API (and compatible endpoints such as
/// LM Studio).
///
/// Implements [`ChatClient`] so higher-level components (e.g. [`super::LlmQueryExpander`])
/// stay decoupled from transport and serialization details.
///
/// **Local-first defaults**: targets LM Studio on `http://localhost:1234` without
/// an API key.  Override via environment variables to target the Anthropic cloud:
///
/// ```text
/// ANTHROPIC_BASE_URL=https://api.anthropic.com
/// ANTHROPIC_API_KEY=sk-ant-...
/// ANTHROPIC_MODEL=claude-haiku-4-5
/// ```
///
/// Before each request the client sends a lightweight `HEAD /` probe with a
/// 2-second timeout.  If the server isn't reachable (connection refused or
/// probe timeout) the call fails immediately instead of hanging for 30 s.
pub struct AnthropicClient {
    client: reqwest::Client,
    /// Cheap connectivity check — short timeout, discards the response body.
    probe_client: reqwest::Client,
    api_key: String,
    model: String,
    /// Full endpoint URL (base + MESSAGES_PATH).
    url: String,
    /// Base URL used for the probe (e.g. `http://localhost:1234/`).
    base_url: String,
}

impl AnthropicClient {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let base: String = base_url.into();
        let trimmed = base.trim_end_matches('/');
        let url = format!("{trimmed}{MESSAGES_PATH}");
        let base_url = format!("{trimmed}/");
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            probe_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            model: model.into(),
            url,
            base_url,
        }
    }

    /// Construct from environment variables with local-first defaults:
    ///
    /// | Variable             | Default                   | Purpose                   |
    /// |----------------------|---------------------------|---------------------------|
    /// | `ANTHROPIC_BASE_URL` | `http://localhost:1234`   | LM Studio / any server    |
    /// | `ANTHROPIC_MODEL`    | `ministral-3b-2512`       | Model in LM Studio        |
    /// | `ANTHROPIC_API_KEY`  | `""` (empty)              | Not required for local    |
    pub fn from_env() -> Self {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        Self::new(key, model, base)
    }

    /// Return the configured base URL (for logging purposes).
    pub fn configured_base_url() -> String {
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
    }
}

#[async_trait]
impl ChatClient for AnthropicClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        // Fast connectivity probe: HEAD / with a 2-second timeout.
        // Fails immediately on connection-refused or probe timeout so callers
        // don't wait 30 s when LM Studio (or any local server) isn't running.
        // Any HTTP response — even 4xx/5xx — means the server is up; proceed.
        match self.probe_client.head(&self.base_url).send().await {
            Err(e) if e.is_connect() || e.is_timeout() => {
                return Err(DomainError::StorageError(format!(
                    "AnthropicClient: server not reachable at {}: {e}",
                    self.base_url.trim_end_matches('/')
                )));
            }
            _ => {}
        }

        let request = ApiRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            system,
            messages: vec![ApiMessage {
                role: "user",
                content: user,
            }],
        };

        let response = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(&request)
            .send()
            .await
            .map_err(|e| DomainError::StorageError(format!("AnthropicClient: request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("AnthropicClient: API returned {status}: {body}");
            return Err(DomainError::StorageError(format!(
                "AnthropicClient: API returned {status}"
            )));
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .map_err(|e| DomainError::StorageError(format!("AnthropicClient: failed to parse response: {e}")))?;

        Ok(api_response
            .content
            .into_iter()
            .next()
            .map(|b| b.text)
            .unwrap_or_default())
    }
}
