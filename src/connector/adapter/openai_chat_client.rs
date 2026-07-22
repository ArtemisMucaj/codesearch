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

// ---------------------------------------------------------------------------
// Responses API (`/responses`) types.
//
// Some models are reachable only via the newer OpenAI Responses API and reject
// `/chat/completions` (GitHub Copilot's GPT-5.x family), while others are the
// reverse. LM Studio serves both. The request/response shapes differ from chat,
// so we model the subset we need and translate to/from the same
// `system`/`user` → text contract the chat path uses.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    /// Structured input turns (role + content). The Responses API also accepts a
    /// bare string, but the turn form lets us carry the system prompt cleanly.
    input: Vec<ResponsesInputItem>,
    stream: bool,
}

#[derive(Serialize)]
struct ResponsesInputItem {
    role: String,
    content: String,
}

/// Non-streaming Responses body: `output` is a list of items; assistant text
/// lives in `message`-type items' `content[]` as `output_text` parts.
#[derive(Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    /// `message`, `reasoning`, … — only `message` carries user-facing text.
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<ResponsesContentPart>,
}

#[derive(Deserialize)]
struct ResponsesContentPart {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl ResponsesResponse {
    /// Concatenate the `output_text` parts of every `message` item — the
    /// assistant's answer, skipping reasoning/tool items.
    fn into_text(self) -> Option<String> {
        let text: String = self
            .output
            .into_iter()
            .filter(|item| item.kind == "message" || item.kind.is_empty())
            .flat_map(|item| item.content)
            .filter(|part| part.kind == "output_text")
            .map(|part| part.text)
            .collect();
        (!text.trim().is_empty()).then_some(text)
    }
}

/// One SSE event from a streaming Responses call. We only care about the
/// incremental text deltas (`response.output_text.delta`); other event types
/// (created, reasoning, completed, …) are ignored.
#[derive(Deserialize)]
struct ResponsesStreamEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
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
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("OpenAiChatClient: failed to read models error-response body: {e}");
                    format!("<failed to read body: {e}>")
                }
            };
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

/// Most attempts against Copilot's intermittently-gated `/responses` endpoint
/// (it returns 403 ~half the time regardless of headers — a GitHub-side rollout
/// gate, not an auth problem).
const RESPONSES_403_RETRIES: usize = 4;

/// Backoff between `/responses` 403 retries.
const RESPONSES_403_BACKOFF: Duration = Duration::from_millis(400);

impl OpenAiChatClient {
    /// The `/responses` URL for this client, derived from the chat URL by
    /// swapping the `chat/completions` suffix for `responses`. Works for both
    /// path conventions: LM Studio's `/v1/chat/completions` → `/v1/responses`
    /// and Copilot's `/chat/completions` → `/responses`.
    fn responses_url(&self) -> String {
        self.url.replace("chat/completions", "responses")
    }

    /// Whether an error body signals the model is on the *other* API — GitHub
    /// Copilot returns `code: "unsupported_api_for_model"` from both endpoints
    /// (chat rejects Responses-only models and vice-versa), so this is the
    /// switch-endpoints signal.
    fn is_wrong_endpoint(body: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .map(|c| c == "unsupported_api_for_model")
            })
            .unwrap_or(false)
    }

    /// Non-streaming completion via the Responses API. Retries the intermittent
    /// Copilot 403 a few times. A 4xx carrying `unsupported_api_for_model` means
    /// the model wants chat instead — surfaced as [`CompletionError::WrongEndpoint`].
    async fn complete_via_responses(
        &self,
        system: &str,
        user: &str,
    ) -> Result<String, CompletionError> {
        let body = ResponsesRequest {
            model: self.model.clone(),
            input: vec![
                ResponsesInputItem {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                ResponsesInputItem {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            stream: false,
        };
        let url = self.responses_url();

        for attempt in 0..=RESPONSES_403_RETRIES {
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    CompletionError::Fatal(DomainError::internal(format!(
                        "Responses API request failed: {e}"
                    )))
                })?;

            let status = resp.status();
            if status.is_success() {
                let parsed: ResponsesResponse = resp.json().await.map_err(|e| {
                    CompletionError::Fatal(DomainError::internal(format!(
                        "Failed to parse Responses API response: {e}"
                    )))
                })?;
                return parsed.into_text().ok_or_else(|| {
                    CompletionError::Fatal(DomainError::internal(
                        "Responses API returned empty content",
                    ))
                });
            }

            // The endpoint is gated intermittently; a 403 is worth retrying.
            if status == reqwest::StatusCode::FORBIDDEN && attempt < RESPONSES_403_RETRIES {
                debug!("Responses API 403 (attempt {}), retrying", attempt + 1);
                tokio::time::sleep(RESPONSES_403_BACKOFF).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            if Self::is_wrong_endpoint(&text) {
                return Err(CompletionError::WrongEndpoint);
            }
            return Err(CompletionError::Fatal(DomainError::internal(format!(
                "Responses API returned {status}: {text}"
            ))));
        }
        Err(CompletionError::Fatal(DomainError::internal(
            "Responses API kept returning 403 after retries",
        )))
    }

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
            // The model is Responses-only; signal a retry on that endpoint.
            if Self::is_wrong_endpoint(&body) {
                return Err(CompletionError::WrongEndpoint);
            }
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
    /// The model is served by the *other* API (chat ↔ responses); retry there.
    WrongEndpoint,
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
            CompletionError::WrongEndpoint => {
                DomainError::internal("model is on the other API and no fallback was attempted")
            }
        }
    }
}

#[async_trait]
impl ChatClient for OpenAiChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        // No response_format ⇒ FormatUnsupported is impossible here.
        match self.complete_with_format(system, user, None).await {
            Ok(text) => Ok(text),
            // The model is Responses-only — retry there.
            Err(CompletionError::WrongEndpoint) => self
                .complete_via_responses(system, user)
                .await
                .map_err(CompletionError::into_fatal),
            Err(other) => Err(other.into_fatal()),
        }
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
                match self.complete_with_format(system, user, None).await {
                    Ok(text) => Ok(text),
                    // Even unconstrained, chat rejects a Responses-only model.
                    Err(CompletionError::WrongEndpoint) => self
                        .complete_via_responses(system, user)
                        .await
                        .map_err(CompletionError::into_fatal),
                    Err(e) => Err(e.into_fatal()),
                }
            }
            // The model is Responses-only. The Responses API has no portable
            // schema-constraint, so we send it unconstrained and rely on the
            // caller's tolerant parser — same as the FormatUnsupported path.
            Err(CompletionError::WrongEndpoint) => self
                .complete_via_responses(system, user)
                .await
                .map_err(CompletionError::into_fatal),
            Err(e) => Err(e.into_fatal()),
        }
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        match self.chat_stream(system, user, &token_tx).await {
            Ok(text) => Ok(text),
            // The model is Responses-only — stream from that endpoint instead.
            // Safe to retry because a WrongEndpoint is only returned before any
            // token was emitted (it's detected on the initial response status).
            Err(CompletionError::WrongEndpoint) => {
                self.responses_stream(system, user, &token_tx).await
            }
            Err(other) => Err(other.into_fatal()),
        }
    }
}

impl OpenAiChatClient {
    /// Stream a completion from `/chat/completions`. Returns
    /// [`CompletionError::WrongEndpoint`] (before emitting any token) when the
    /// model is Responses-only, so [`Self::complete_stream`] can fall back.
    async fn chat_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: &UnboundedSender<String>,
    ) -> Result<String, CompletionError> {
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
                CompletionError::Fatal(DomainError::internal(format!(
                    "OpenAI chat stream request failed: {e}"
                )))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
            if Self::is_wrong_endpoint(&body) {
                return Err(CompletionError::WrongEndpoint);
            }
            return Err(CompletionError::Fatal(DomainError::internal(format!(
                "OpenAI chat API returned {status}: {body}"
            ))));
        }

        let mut byte_stream = resp.bytes_stream();
        let mut full_text = String::new();
        // Accumulate bytes until we have a complete SSE line.
        let mut buffer = String::new();

        'outer: while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| {
                CompletionError::Fatal(DomainError::internal(format!(
                    "OpenAI stream read error: {e}"
                )))
            })?;
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

    /// Stream a completion from the `/responses` endpoint, forwarding
    /// `output_text.delta` events as tokens. Retries the intermittent Copilot
    /// 403 before it commits to reading the stream.
    async fn responses_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: &UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        let body = ResponsesRequest {
            model: self.model.clone(),
            input: vec![
                ResponsesInputItem {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                ResponsesInputItem {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            stream: true,
        };
        let url = self.responses_url();

        let mut attempt = 0;
        let resp = loop {
            let r = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal(format!("Responses API stream request failed: {e}"))
                })?;
            if r.status().is_success() {
                break r;
            }
            if r.status() == reqwest::StatusCode::FORBIDDEN && attempt < RESPONSES_403_RETRIES {
                attempt += 1;
                debug!("Responses API stream 403 (attempt {attempt}), retrying");
                tokio::time::sleep(RESPONSES_403_BACKOFF).await;
                continue;
            }
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            return Err(DomainError::internal(format!(
                "Responses API returned {status}: {text}"
            )));
        };

        let mut byte_stream = resp.bytes_stream();
        let mut full_text = String::new();
        let mut buffer = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk
                .map_err(|e| DomainError::internal(format!("Responses stream read error: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim_end_matches('\r').to_string();
                buffer = buffer[newline + 1..].to_string();

                // The Responses SSE stream carries the payload on `data:` lines;
                // the `event:` line duplicates the `type` inside that JSON, so we
                // key off the JSON's own `type` field and forward text deltas.
                let Some(data) = line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<ResponsesStreamEvent>(data.trim()) else {
                    continue;
                };
                if event.kind == "response.output_text.delta" {
                    if let Some(text) = event.delta {
                        full_text.push_str(&text);
                        let _ = token_tx.send(text);
                    }
                }
            }
        }

        Ok(full_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_wrong_endpoint_error() {
        let chat_err = r#"{"error":{"message":"model \"gpt-5.6-luna\" is not accessible via the /chat/completions endpoint","code":"unsupported_api_for_model"}}"#;
        let responses_err = r#"{"error":{"message":"model claude-opus-4.8 does not support Responses API.","code":"unsupported_api_for_model"}}"#;
        assert!(OpenAiChatClient::is_wrong_endpoint(chat_err));
        assert!(OpenAiChatClient::is_wrong_endpoint(responses_err));
        // A different failure (bad request, auth, malformed) must NOT trigger a
        // pointless endpoint switch.
        assert!(!OpenAiChatClient::is_wrong_endpoint(
            r#"{"error":{"code":"invalid_request_error"}}"#
        ));
        assert!(!OpenAiChatClient::is_wrong_endpoint("not json at all"));
    }

    #[test]
    fn derives_responses_url_for_both_conventions() {
        // Copilot: no /v1 prefix.
        let copilot = OpenAiChatClient::with_parts(
            reqwest::Client::new(),
            "https://api.githubcopilot.com/chat/completions".to_string(),
            "gpt-5.6-luna".to_string(),
        );
        assert_eq!(
            copilot.responses_url(),
            "https://api.githubcopilot.com/responses"
        );
        // LM Studio / OpenAI: /v1 prefix.
        let lmstudio = OpenAiChatClient::with_parts(
            reqwest::Client::new(),
            "http://localhost:1234/v1/chat/completions".to_string(),
            "m".to_string(),
        );
        assert_eq!(
            lmstudio.responses_url(),
            "http://localhost:1234/v1/responses"
        );
    }

    #[test]
    fn parses_responses_output_text() {
        // Only `message` items' `output_text` parts contribute; reasoning items
        // and non-text parts are skipped.
        let json = r#"{
            "output": [
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"thinking..."}]},
                {"type":"message","content":[
                    {"type":"output_text","text":"Hello"},
                    {"type":"output_text","text":", world"}
                ]}
            ]
        }"#;
        let parsed: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.into_text().as_deref(), Some("Hello, world"));
    }

    #[test]
    fn empty_responses_output_is_none() {
        let parsed: ResponsesResponse = serde_json::from_str(r#"{"output":[]}"#).unwrap();
        assert_eq!(parsed.into_text(), None);
    }
}
