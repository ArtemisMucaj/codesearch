use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
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
/// The first call to `complete` probes the base URL with a 2-second timeout.
/// The result is cached: reachable → all future calls skip the probe; not
/// reachable → all future calls fail immediately without touching the network.
pub struct AnthropicClient {
    client: reqwest::Client,
    /// Short-timeout client used only for the one-time connectivity probe.
    probe_client: reqwest::Client,
    api_key: String,
    model: String,
    /// Full endpoint URL (base + MESSAGES_PATH).
    url: String,
    /// Base URL sent to the probe (e.g. `http://localhost:1234/`).
    base_url: String,
    /// Cached probe outcome: `Ok(())` = reachable, `Err(msg)` = not reachable.
    reachable: tokio::sync::OnceCell<Result<(), String>>,
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
                .expect("reqwest::Client with 30-second timeout failed to build"),
            probe_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(2))
                .build()
                .expect("reqwest::Client with 2-second probe timeout failed to build"),
            api_key: api_key.into(),
            model: model.into(),
            url,
            base_url,
            reachable: tokio::sync::OnceCell::new(),
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

    /// Return the base URL this instance was constructed with (for logging).
    pub fn configured_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

#[async_trait]
impl ChatClient for AnthropicClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        // Connectivity probe — runs exactly once; result is cached for all
        // subsequent calls.  On connection-refused / timeout we fail fast
        // instead of hanging for the full 30-second request timeout.
        // Any HTTP response (even 4xx/5xx) means the server is up.
        let probe_client = &self.probe_client;
        let base_url = &self.base_url;
        let probe = self.reachable.get_or_init(|| async move {
            match probe_client.head(base_url).send().await {
                Ok(_) => Ok(()),
                Err(e) if e.is_connect() || e.is_timeout() => Err(format!(
                    "server not reachable at {}: {e}",
                    base_url.trim_end_matches('/')
                )),
                Err(e) => {
                    warn!(
                        "AnthropicClient: probe to {} failed unexpectedly: {e}",
                        base_url.trim_end_matches('/')
                    );
                    Err(format!(
                        "probe to {} failed: {e}",
                        base_url.trim_end_matches('/')
                    ))
                }
            }
        }).await;
        if let Err(msg) = probe {
            return Err(DomainError::StorageError(format!("AnthropicClient: {msg}")));
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
            let total = body.len();
            let snippet = match body.char_indices().nth(1000) {
                Some((i, _)) => format!("{}...(truncated, total {total} bytes)", &body[..i]),
                None => body,
            };
            warn!("AnthropicClient: API returned {status}: {snippet}");
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
