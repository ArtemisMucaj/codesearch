use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

/// Default target: LM Studio running locally on its standard port.
pub const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const MESSAGES_PATH: &str = "/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Default model matches the LM Studio local-first default.
const DEFAULT_MODEL: &str = "mistralai/ministral-3-3b";
/// Budget is set high so that thinking-mode models (e.g. Qwen with extended
/// thinking) can spend tokens on their `<think>` pass without exhausting the
/// budget before the actual XML response is written.
const MAX_TOKENS: u32 = 16_384;

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
struct ApiStreamRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
    stream: bool,
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

/// A single item in the Anthropic `content` array.
///
/// Models running in thinking mode (e.g. Qwen with extended thinking) emit a
/// `{"type":"thinking","thinking":"..."}` block **before** the real
/// `{"type":"text","text":"..."}` block.  Serde will fail on the whole array
/// if it encounters an unknown type, so we enumerate all known variants and
/// add a catch-all for anything else.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ContentBlock {
    /// The actual model response we care about.
    Text { text: String },
    /// Thinking / scratchpad block emitted by reasoning models — discarded.
    Thinking {
        #[allow(dead_code)]
        thinking: String,
    },
    /// Any other block type (e.g. `tool_use`, future variants) — discarded.
    #[serde(other)]
    Unknown,
}

/// SSE event emitted by the Anthropic streaming API.
///
/// Only `content_block_delta` events with a `text_delta` are meaningful to us;
/// all other event types are silently discarded.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    ContentBlockDelta {
        #[allow(dead_code)]
        index: u32,
        delta: StreamDelta,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamDelta {
    TextDelta { text: String },
    #[serde(other)]
    Other,
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
                .timeout(Duration::from_secs(300))
                .build()
                .expect("reqwest::Client with 300-second timeout failed to build"),
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
    /// | `ANTHROPIC_MODEL`    | `mistralai/ministral-3-3b` | Model in LM Studio       |
    /// | `ANTHROPIC_API_KEY`  | `""` (empty)              | Not required for local    |
    pub fn from_env() -> Self {
        let base =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        Self::new(key, model, base)
    }

    /// Return the base URL this instance was constructed with (for logging).
    pub fn configured_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Run the connectivity probe exactly once and return the cached result.
    async fn probe(&self) -> Result<(), DomainError> {
        let probe_client = &self.probe_client;
        let base_url = &self.base_url;
        let outcome = self
            .reachable
            .get_or_init(|| async move {
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
            })
            .await;
        if let Err(msg) = outcome {
            return Err(DomainError::StorageError(format!("AnthropicClient: {msg}")));
        }
        Ok(())
    }
}

#[async_trait]
impl ChatClient for AnthropicClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        self.probe().await?;

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
            .map_err(|e| {
                DomainError::StorageError(format!("AnthropicClient: request failed: {e}"))
            })?;

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

        let api_response: ApiResponse = response.json().await.map_err(|e| {
            DomainError::StorageError(format!("AnthropicClient: failed to parse response: {e}"))
        })?;

        Ok(api_response
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""))
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        self.probe().await?;

        let request = ApiStreamRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            system,
            messages: vec![ApiMessage {
                role: "user",
                content: user,
            }],
            stream: true,
        };

        let response = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                DomainError::StorageError(format!("AnthropicClient: request failed: {e}"))
            })?;

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

        let mut byte_stream = response.bytes_stream();
        let mut full_text = String::new();
        // Accumulate bytes until we have a complete SSE event (terminated by \n\n).
        let mut buffer = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| {
                DomainError::StorageError(format!("AnthropicClient: stream read error: {e}"))
            })?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process all complete SSE events in the buffer.
            while let Some(boundary) = buffer.find("\n\n") {
                let event_str = buffer[..boundary].to_string();
                buffer = buffer[boundary + 2..].to_string();
                for line in event_str.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(event) = serde_json::from_str::<StreamEvent>(data) else {
                        continue;
                    };
                    if let StreamEvent::ContentBlockDelta {
                        delta: StreamDelta::TextDelta { text },
                        ..
                    } = event
                    {
                        full_text.push_str(&text);
                        let _ = token_tx.send(text);
                    }
                }
            }
        }

        Ok(full_text)
    }
}
