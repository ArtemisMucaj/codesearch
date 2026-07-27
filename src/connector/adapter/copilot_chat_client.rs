//! [`ChatClient`] backed by a **GitHub Copilot subscription**, over direct HTTP.
//!
//! The Copilot API is OpenAI-compatible but served at the root (no `/v1`) behind
//! a set of client-identity headers. That Copilot-specific knowledge lives in
//! [`gh_copilot_rs`]; this adapter wires it to an [`OpenAiChatClient`] so the
//! shared chat/stream logic (Responses-first, structured output, SSE) is reused
//! rather than duplicated. Model discovery goes through the crate's
//! [`CopilotModelCatalog`](gh_copilot_rs::CopilotModelCatalog), whose richer
//! metadata the picker and `/api/llm/models` surface.
//!
//! Auth is the GitHub OAuth **device flow** (from `gh-copilot-rs`) run by
//! `codesearch copilot login`; the captured `ghu_…` token is read from
//! `<data_dir>/config.json`.

use async_trait::async_trait;
use gh_copilot_rs::{CopilotEndpoint, CopilotModelCatalog, CopilotToken};
use openai_rs::{ApiRoutes, Endpoint, Transport};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::connector::adapter::{ChatClient, OpenAiChatClient};
use crate::domain::DomainError;

// Re-export the crate's Copilot metadata types under their historical codesearch
// names, so the model picker and CLI/API response mappers keep compiling with
// only an import change.
pub use gh_copilot_rs::{
    CopilotModel, CopilotModelCapabilities, CopilotModelLimits, COPILOT_API_BASE,
};

/// [`ChatClient`] that routes completions through a GitHub Copilot subscription
/// via direct HTTP to the Copilot API.
pub struct CopilotChatClient {
    /// Delegate carrying the shared OpenAI-compatible chat/stream logic, built
    /// against the Copilot base URL with the auth + Copilot headers baked in.
    inner: OpenAiChatClient,
    /// Copilot endpoint description (base URL, headers, credential), reused for
    /// the `/models` catalog call.
    endpoint: CopilotEndpoint,
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
        let endpoint = CopilotEndpoint::from_optional_token(github_token.map(CopilotToken::new));
        let model_id = model.clone().unwrap_or_default();
        debug!(
            "CopilotChatClient: endpoint={}, model={model_id:?}",
            endpoint.base_url()
        );

        // Wire the Copilot endpoint to an OpenAI-compatible client: root-served
        // routes, the Copilot protocol headers, and the token as the bearer key.
        let openai_endpoint = Endpoint::new(endpoint.base_url())
            .with_routes(ApiRoutes::unversioned())
            .with_headers(endpoint.protocol_headers())
            .with_timeout(endpoint.timeout())
            .with_optional_api_key(endpoint.token().map(|t| t.expose().to_string()));
        let transport = Transport::new(&openai_endpoint)?;
        let inner = OpenAiChatClient::with_transport(transport, model_id);

        Ok(Self {
            inner,
            endpoint,
            model,
        })
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

    /// Whether a usable Copilot credential is present. Lets a handler surface an
    /// actionable "not authenticated" error instead of an opaque 401/500.
    pub fn is_authenticated(&self) -> bool {
        self.endpoint.is_authenticated()
    }

    /// List the models available to the authenticated Copilot account. Backs
    /// `codesearch copilot models`, the login-TUI picker, and the serve-mode
    /// `GET /api/llm/models` endpoint.
    pub async fn list_models(&self) -> Result<Vec<CopilotModel>, DomainError> {
        let token = self
            .endpoint
            .token()
            .cloned()
            .ok_or_else(|| DomainError::internal("Copilot is not authenticated"))?;
        let catalog = CopilotModelCatalog::new(token)?;
        Ok(catalog.list_models().await?)
    }
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
