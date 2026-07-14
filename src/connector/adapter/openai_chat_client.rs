use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const CHAT_PATH: &str = "/v1/chat/completions";
/// OpenAI-compatible model-discovery endpoint (`GET`). Derived from the base
/// URL, so it works against LM Studio, OpenAI, and any compatible server.
const MODELS_PATH: &str = "/v1/models";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default model when neither the endpoint config nor `OPENAI_MODEL` sets one.
const DEFAULT_MODEL: &str = "google/gemma-4-e2b";

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
    /// Optional structured-output constraint (OpenAI-compatible
    /// `response_format`). Omitted from the request body when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

/// `response_format: { type: "json_schema", json_schema: { … } }` — asks an
/// OpenAI-compatible server to grammar-constrain output to the given schema.
#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchemaSpec,
}

#[derive(Serialize)]
struct JsonSchemaSpec {
    name: String,
    /// Reject any output that does not match the schema exactly.
    strict: bool,
    schema: serde_json::Value,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    #[serde(default)]
    content: Option<String>,
    /// Some reasoning models (e.g. Qwen3.5) route the whole answer into a
    /// separate `reasoning_content` channel and leave `content` empty. We fall
    /// back to it so the response isn't lost.
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl ChatResponseMessage {
    /// The assistant's text: `content` when present and non-empty, else the
    /// reasoning channel (for reasoning models that leave `content` empty).
    fn into_text(self) -> Option<String> {
        let content = self.content.filter(|c| !c.trim().is_empty());
        content.or_else(|| self.reasoning_content.filter(|c| !c.trim().is_empty()))
    }
}

/// Response of `GET /v1/models` on an OpenAI-compatible server.
#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

/// One entry in the `/v1/models` list. Only `id` is required by the spec; the
/// rest is server-specific and ignored here.
#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// A single chunk from an OpenAI-compatible streaming response.
#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

/// [`ChatClient`] implementation targeting the OpenAI-compatible
/// `/v1/chat/completions` endpoint (e.g. LM Studio running locally).
///
/// **Configuration** (via environment variables):
///
/// | Variable          | Default                    |
/// |-------------------|----------------------------|
/// | `OPENAI_BASE_URL` | `http://localhost:1234`    |
/// | `OPENAI_MODEL`    | `google/gemma-4-e2b`       |
/// | `OPENAI_API_KEY`  | `""` (not required locally)|
pub struct OpenAiChatClient {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl OpenAiChatClient {
    /// Build from the `OPENAI_*` environment variables (the default endpoint
    /// when no named endpoint from config is selected).
    pub fn from_env() -> Result<Self, reqwest::Error> {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        let timeout_secs = std::env::var("OPENAI_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Self::from_endpoint(&base, &model, api_key.as_deref(), timeout_secs)
    }

    /// Build the OpenAI-compatible client for a run: resolve a named endpoint
    /// from `<data_dir>/config.json` (honoring `endpoint_override`, then the
    /// configured `active` endpoint), falling back to the `OPENAI_*`
    /// environment variables when no endpoint is configured.
    pub fn from_config(
        data_dir: &str,
        endpoint_override: Option<&str>,
    ) -> Result<Self, DomainError> {
        let cfg = super::CodesearchConfig::load(data_dir)?;
        match cfg.resolve_openai_endpoint(endpoint_override) {
            Some(ep) => {
                let model = ep.model.as_deref().unwrap_or(DEFAULT_MODEL);
                Self::from_endpoint(
                    &ep.base_url,
                    model,
                    ep.api_key.as_deref(),
                    DEFAULT_TIMEOUT_SECS,
                )
                .map_err(|e| DomainError::internal(format!("failed to build OpenAI client: {e}")))
            }
            None => Self::from_env()
                .map_err(|e| DomainError::internal(format!("failed to build OpenAI client: {e}"))),
        }
    }

    /// Build from an explicit endpoint (a named endpoint from config): `base`
    /// URL, `model`, and optional bearer `api_key`. Shares the header/client
    /// construction with [`Self::from_env`].
    pub fn from_endpoint(
        base: &str,
        model: &str,
        api_key: Option<&str>,
        timeout_secs: u64,
    ) -> Result<Self, reqwest::Error> {
        let url = format!("{}{}", base.trim_end_matches('/'), CHAT_PATH);
        debug!("OpenAiChatClient: endpoint={}, model={}", url, model);

        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            match reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                Ok(val) => {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
                Err(e) => {
                    let masked = mask_key(key);
                    warn!(
                        "OpenAiChatClient: failed to build Authorization header \
                         (key={masked}): {e}; skipping"
                    );
                }
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .default_headers(headers)
            .build()?;

        Ok(Self {
            client,
            url,
            model: model.to_string(),
        })
    }

    /// Construct from pre-built parts: a `reqwest::Client` (whose default
    /// headers already carry any auth), the full chat-completions `url`, and the
    /// `model` to send. Used by [`CopilotChatClient`](super::CopilotChatClient),
    /// which speaks the same OpenAI-compatible protocol against
    /// `https://api.githubcopilot.com/chat/completions` with Copilot auth
    /// headers — so it reuses all of this client's request/stream logic instead
    /// of duplicating it.
    pub fn with_parts(client: reqwest::Client, url: String, model: String) -> Self {
        Self { client, url, model }
    }

    /// The base URL this client is configured to use — useful for log messages.
    pub fn configured_base_url(&self) -> String {
        self.url.trim_end_matches(CHAT_PATH).to_string()
    }

    /// The model id this client sends in chat requests.
    pub fn configured_model(&self) -> &str {
        &self.model
    }

    /// Discover the models the server offers via `GET /v1/models`.
    ///
    /// Returns their ids (e.g. `"google/gemma-4-e2b"`). Works against any
    /// OpenAI-compatible server (LM Studio, OpenAI, vLLM, …). Errors if the
    /// endpoint is unreachable or returns a non-success status.
    pub async fn list_models(&self) -> Result<Vec<String>, DomainError> {
        let url = format!("{}{}", self.configured_base_url(), MODELS_PATH);
        let resp = self.client.get(&url).send().await.map_err(|e| {
            DomainError::internal(format!("OpenAI models request to {url} failed: {e}"))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DomainError::internal(format!(
                "OpenAI models API returned {status}: {body}"
            )));
        }

        let parsed: ModelsResponse = resp.json().await.map_err(|e| {
            DomainError::internal(format!("failed to parse OpenAI models response: {e}"))
        })?;
        Ok(parsed.data.into_iter().map(|m| m.id).collect())
    }
}

/// Returns a masked version of `key` for logging: first 4 and last 4 chars
/// visible, rest replaced with `*`.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}{}{}", prefix, "*".repeat(chars.len() - 8), suffix)
}

impl OpenAiChatClient {
    /// Shared non-streaming completion: POST the request (optionally with a
    /// `response_format` constraint) and return the assistant's text.
    ///
    /// When a `response_format` was requested and the server rejects it with a
    /// client error (4xx) — as some engines do when a model's grammar cannot
    /// honor the schema — this returns [`CompletionError::FormatUnsupported`] so
    /// the caller can retry without the constraint.
    async fn complete_with_format(
        &self,
        system: &str,
        user: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<String, CompletionError> {
        let constrained = response_format.is_some();
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            temperature: 0.0,
            stream: false,
            response_format,
        };

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                CompletionError::Fatal(DomainError::internal(format!(
                    "OpenAI chat request failed: {e}"
                )))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("OpenAiChatClient: failed to read error response body: {e}");
                    format!("<failed to read body: {e}>")
                }
            };
            // A 4xx on a constrained request means the backend/model could not
            // honor the schema; signal a retry without it rather than failing.
            if constrained && status.is_client_error() {
                return Err(CompletionError::FormatUnsupported);
            }
            return Err(CompletionError::Fatal(DomainError::internal(format!(
                "OpenAI chat API returned {status}: {body}"
            ))));
        }

        let chat: ChatResponse = resp.json().await.map_err(|e| {
            CompletionError::Fatal(DomainError::internal(format!(
                "Failed to parse OpenAI chat response: {e}"
            )))
        })?;

        let message = chat
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| {
                CompletionError::Fatal(DomainError::internal("OpenAI chat returned no choices"))
            })?;
        message.into_text().ok_or_else(|| {
            CompletionError::Fatal(DomainError::internal("OpenAI chat returned empty content"))
        })
    }
}

/// Outcome of a completion attempt that can distinguish a schema rejection
/// (retryable without the constraint) from a genuine failure.
enum CompletionError {
    /// The backend rejected the `response_format`; retry unconstrained.
    FormatUnsupported,
    /// Any other error — propagate.
    Fatal(DomainError),
}

impl CompletionError {
    /// Collapse into a `DomainError`. A `FormatUnsupported` reaching here means
    /// an unconstrained call somehow produced it (it shouldn't); surface it as
    /// an internal error rather than silently swallowing.
    fn into_fatal(self) -> DomainError {
        match self {
            CompletionError::Fatal(e) => e,
            CompletionError::FormatUnsupported => {
                DomainError::internal("unexpected response_format rejection on unconstrained call")
            }
        }
    }
}

#[async_trait]
impl ChatClient for OpenAiChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        // No response_format ⇒ FormatUnsupported is impossible here.
        self.complete_with_format(system, user, None)
            .await
            .map_err(CompletionError::into_fatal)
    }

    async fn complete_json(
        &self,
        system: &str,
        user: &str,
        schema_name: &str,
        schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        let response_format = ResponseFormat {
            kind: "json_schema",
            json_schema: JsonSchemaSpec {
                name: schema_name.to_string(),
                strict: true,
                schema: schema.clone(),
            },
        };
        match self
            .complete_with_format(system, user, Some(response_format))
            .await
        {
            Ok(text) => Ok(text),
            // The backend can't grammar-constrain to this schema (e.g. gemma-4's
            // engine on some LM Studio builds). Fall back to free-form output;
            // the caller's tolerant parser + repair pass handle it.
            Err(CompletionError::FormatUnsupported) => {
                warn!(
                    "OpenAiChatClient: backend rejected response_format for '{schema_name}'; \
                     retrying without structured output"
                );
                self.complete_with_format(system, user, None)
                    .await
                    .map_err(CompletionError::into_fatal)
            }
            Err(e) => Err(e.into_fatal()),
        }
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            temperature: 0.0,
            stream: true,
            response_format: None,
        };

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                DomainError::internal(format!("OpenAI chat stream request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => format!("<failed to read body: {e}>"),
            };
            return Err(DomainError::internal(format!(
                "OpenAI chat API returned {status}: {body}"
            )));
        }

        let mut byte_stream = resp.bytes_stream();
        let mut full_text = String::new();
        // Accumulate bytes until we have a complete SSE line.
        let mut buffer = String::new();

        'outer: while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk
                .map_err(|e| DomainError::internal(format!("OpenAI stream read error: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process all complete lines in the buffer.
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim_end_matches('\r').to_string();
                buffer = buffer[newline + 1..].to_string();

                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.trim() == "[DONE]" {
                    break 'outer;
                }
                let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                    continue;
                };
                if let Some(text) = chunk
                    .choices
                    .into_iter()
                    .next()
                    .and_then(|c| c.delta.content)
                {
                    full_text.push_str(&text);
                    let _ = token_tx.send(text);
                }
            }
        }

        Ok(full_text)
    }
}
