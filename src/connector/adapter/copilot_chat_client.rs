//! [`ChatClient`] backed by a **GitHub Copilot subscription**, over direct HTTP.
//!
//! The Copilot API is OpenAI-compatible (`/chat/completions`, `/models`), so
//! this adapter talks to `https://api.githubcopilot.com` directly with a
//! `reqwest` client whose default headers carry the OAuth `ghu_…` token as a
//! `Bearer` credential plus the Copilot-specific headers. Chat + streaming are
//! delegated to an internal [`OpenAiChatClient`] so all of that request/SSE
//! logic is shared rather than duplicated; only model discovery (Copilot's
//! `/models` returns richer metadata than the OpenAI list) lives here.
//!
//! Auth is the GitHub OAuth **device flow** run by `codesearch copilot login`
//! (see [`super::copilot_auth`]); the captured token is read from
//! `<data_dir>/config.json`.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::connector::adapter::{ChatClient, OpenAiChatClient};
use crate::domain::DomainError;

/// Base URL of the (individual-account) Copilot API.
pub const COPILOT_API_BASE: &str = "https://api.githubcopilot.com";
const CHAT_PATH: &str = "/chat/completions";
const MODELS_PATH: &str = "/models";

/// Copilot API version pinned in the `X-GitHub-Api-Version` header.
const API_VERSION: &str = "2025-04-01";
/// Editor identity the Copilot API expects; mirrors the VS Code Copilot client.
const EDITOR_VERSION: &str = "vscode/1.99.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.0";
const USER_AGENT: &str = "GitHubCopilotChat/0.26.0";
/// Integration id GitHub uses to gate Copilot chat access.
const INTEGRATION_ID: &str = "vscode-chat";

const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Wall-clock budget for listing models, so a stalled call can't hang the
/// `/api/llm/models` request or the login picker indefinitely.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// A model offered to the authenticated Copilot account (`GET /models`). Only
/// the fields codesearch surfaces (picker + API response) are decoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotModel {
    /// Model id to send in chat requests (e.g. `"claude-sonnet-4.5"`).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Vendor/family, when provided.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Whether the model is a preview.
    #[serde(default)]
    pub preview: bool,
    /// Capability limits (context window, etc.).
    #[serde(default)]
    pub capabilities: Option<CopilotModelCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotModelCapabilities {
    #[serde(default)]
    pub limits: Option<CopilotModelLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotModelLimits {
    #[serde(default)]
    pub max_context_window_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

/// Response of `GET /models`.
#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<CopilotModel>,
}

/// [`ChatClient`] that routes completions through a GitHub Copilot subscription
/// via direct HTTP to the Copilot API.
pub struct CopilotChatClient {
    /// Delegate carrying the shared OpenAI-compatible chat/stream logic, built
    /// against the Copilot base URL with the auth + Copilot headers baked in.
    inner: OpenAiChatClient,
    /// Same `reqwest` client (with Copilot headers) for the `/models` call.
    http: reqwest::Client,
    /// Model id requested in chat calls, for logging.
    model: Option<String>,
}

impl CopilotChatClient {
    /// Build a client with an explicit token and model.
    ///
    /// `github_token` is the `ghu_…` OAuth token from `copilot login`. When it
    /// is `None`/empty the requests will be unauthenticated and fail — the
    /// caller is expected to have logged in first.
    pub fn new(github_token: Option<String>, model: Option<String>) -> Result<Self, DomainError> {
        let http = build_http_client(github_token.as_deref())?;
        let model_id = model.clone().unwrap_or_default();
        let url = format!("{COPILOT_API_BASE}{CHAT_PATH}");
        debug!("CopilotChatClient: endpoint={url}, model={model_id:?}");
        let inner = OpenAiChatClient::with_parts(http.clone(), url, model_id);
        Ok(Self { inner, http, model })
    }

    /// Build a client from persisted configuration under `data_dir`
    /// (`<data_dir>/config.json`).
    pub fn from_data_dir(data_dir: &str) -> Result<Self, DomainError> {
        Self::from_data_dir_with_model(data_dir, None)
    }

    /// Like [`Self::from_data_dir`] but applies a per-call model override on top
    /// of the stored selection when `model_override` is `Some` — the path used
    /// by serve-mode requests that pick a model on the fly.
    pub fn from_data_dir_with_model(
        data_dir: &str,
        model_override: Option<String>,
    ) -> Result<Self, DomainError> {
        let copilot = super::CodesearchConfig::load_copilot(data_dir)?;
        let model = model_override.or(copilot.model);
        Self::new(copilot.github_token, model)
    }

    /// The model id this client is configured to request, if any (for logging).
    pub fn configured_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// List the models available to the authenticated Copilot account. Backs
    /// `codesearch copilot models`, the login-TUI picker, and the serve-mode
    /// `GET /api/llm/models` endpoint. Bounded by [`LIST_MODELS_TIMEOUT`].
    pub async fn list_models(&self) -> Result<Vec<CopilotModel>, DomainError> {
        let url = format!("{COPILOT_API_BASE}{MODELS_PATH}");
        let fetch = async {
            let resp = self.http.get(&url).send().await.map_err(|e| {
                DomainError::internal(format!("Copilot models request to {url} failed: {e}"))
            })?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(DomainError::internal(format!(
                    "Copilot models API returned {status}: {body}"
                )));
            }
            let parsed: ModelsResponse = resp.json().await.map_err(|e| {
                DomainError::internal(format!("failed to parse Copilot models response: {e}"))
            })?;
            Ok(parsed.data)
        };

        tokio::time::timeout(LIST_MODELS_TIMEOUT, fetch)
            .await
            .map_err(|_| {
                DomainError::internal(format!(
                    "listing Copilot models timed out after {}s",
                    LIST_MODELS_TIMEOUT.as_secs()
                ))
            })?
    }
}

/// Build the `reqwest::Client` with the Copilot auth + protocol headers as
/// defaults, so every request (chat, stream, models) carries them.
fn build_http_client(github_token: Option<&str>) -> Result<reqwest::Client, DomainError> {
    let mut headers = HeaderMap::new();
    if let Some(token) = github_token.filter(|t| !t.is_empty()) {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| DomainError::internal(format!("invalid Copilot token: {e}")))?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    // Static Copilot headers. All values are known-good ASCII, so the
    // `from_static` conversions cannot fail.
    headers.insert(
        "Copilot-Integration-Id",
        HeaderValue::from_static(INTEGRATION_ID),
    );
    headers.insert("Editor-Version", HeaderValue::from_static(EDITOR_VERSION));
    headers.insert(
        "Editor-Plugin-Version",
        HeaderValue::from_static(EDITOR_PLUGIN_VERSION),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(API_VERSION),
    );
    headers.insert(
        "Openai-Intent",
        HeaderValue::from_static("conversation-panel"),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );

    reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .default_headers(headers)
        .build()
        .map_err(|e| DomainError::internal(format!("failed to build Copilot HTTP client: {e}")))
}

#[async_trait]
impl ChatClient for CopilotChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        self.inner.complete(system, user).await
    }

    async fn complete_json(
        &self,
        system: &str,
        user: &str,
        schema_name: &str,
        schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        self.inner
            .complete_json(system, user, schema_name, schema)
            .await
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        self.inner.complete_stream(system, user, token_tx).await
    }
}
