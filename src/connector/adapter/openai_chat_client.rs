use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const CHAT_PATH: &str = "/v1/chat/completions";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
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
    content: String,
}

/// [`ChatClient`] implementation targeting the OpenAI-compatible
/// `/v1/chat/completions` endpoint (e.g. LM Studio running locally).
///
/// **Configuration** (via environment variables):
///
/// | Variable          | Default                    |
/// |-------------------|----------------------------|
/// | `OPENAI_BASE_URL` | `http://localhost:1234`    |
/// | `OPENAI_MODEL`    | `openai-chat`              |
/// | `OPENAI_API_KEY`  | `""` (not required locally)|
pub struct OpenAiChatClient {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl OpenAiChatClient {
    pub fn from_env() -> Result<Self, reqwest::Error> {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let url = format!("{}{}", base.trim_end_matches('/'), CHAT_PATH);
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "openai-chat".to_string());

        debug!("OpenAiChatClient: endpoint={}, model={}", url, model);

        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            if !key.is_empty() {
                match reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                    Ok(val) => {
                        headers.insert(reqwest::header::AUTHORIZATION, val);
                    }
                    Err(e) => {
                        let masked = mask_key(&key);
                        warn!(
                            "OpenAiChatClient: failed to build Authorization header \
                             (key={masked}): {e}; skipping"
                        );
                    }
                }
            }
        }

        let timeout_secs = std::env::var("OPENAI_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .default_headers(headers)
            .build()?;

        Ok(Self { client, url, model })
    }

    /// The base URL this client is configured to use — useful for log messages.
    pub fn configured_base_url(&self) -> String {
        self.url.trim_end_matches(CHAT_PATH).to_string()
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

#[async_trait]
impl ChatClient for OpenAiChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
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
        };

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| DomainError::internal(format!("OpenAI chat request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("OpenAiChatClient: failed to read error response body: {e}");
                    format!("<failed to read body: {e}>")
                }
            };
            return Err(DomainError::internal(format!(
                "OpenAI chat API returned {status}: {body}"
            )));
        }

        let chat: ChatResponse = resp.json().await.map_err(|e| {
            DomainError::internal(format!("Failed to parse OpenAI chat response: {e}"))
        })?;

        chat.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| DomainError::internal("OpenAI chat returned no choices"))
    }
}
