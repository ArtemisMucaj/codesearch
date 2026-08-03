//! [`ChatClient`] targeting OpenAI-compatible servers, backed by the
//! standalone [`openai_rs`] crate.
//!
//! The protocol (Responses API first, Chat Completions fallback, structured
//! output, streaming, model discovery) lives in the crate. This adapter is the
//! thin codesearch boundary: it resolves credentials from `config.json` /
//! `OPENAI_*` (which the crate deliberately never touches), builds the crate
//! client, implements codesearch's [`ChatClient`] port over it, and converts
//! [`openai_rs::OpenAiError`] into [`DomainError`] via
//! [`map_openai_err`](super::map_openai_err) — the conversion lives here rather
//! than as a `From` impl in the domain layer, which stays crate-free.
//!
//! **Determinism.** codesearch's JSON-extraction paths (community naming,
//! execution-feature naming) rely on a fixed temperature, so every request this
//! adapter builds pins `temperature = 0.0` — the crate omits temperature by
//! default (reasoning models reject an explicit one), so the adapter opts in.

use std::time::Duration;

use async_trait::async_trait;
use openai_rs::{
    ChatClient as CrateChatClientPort, ChatRequest, Endpoint, JsonSchema, ModelCatalog,
    OpenAiChatClient as CrateChatClient, OpenAiModelCatalog, Transport,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::connector::adapter::ChatClient;
use crate::domain::DomainError;

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default model when neither the endpoint config nor `OPENAI_MODEL` sets one.
const DEFAULT_MODEL: &str = "google/gemma-4-e2b";
/// Deterministic temperature pinned on every request (see module docs).
const DETERMINISTIC_TEMPERATURE: f32 = 0.0;

/// [`ChatClient`] implementation targeting the OpenAI-compatible protocol (e.g.
/// LM Studio running locally), delegating to [`openai_rs`].
///
/// **Configuration** (via environment variables):
///
/// | Variable          | Default                    |
/// |-------------------|----------------------------|
/// | `OPENAI_BASE_URL` | `http://localhost:1234`    |
/// | `OPENAI_MODEL`    | `google/gemma-4-e2b`       |
/// | `OPENAI_API_KEY`  | `""` (not required locally)|
pub struct OpenAiChatClient {
    inner: CrateChatClient,
    base_url: String,
    /// The fully configured endpoint (api key + timeout included) when this
    /// client was built from one. `list_models` reuses it so model discovery is
    /// authenticated and timed exactly like `complete` — rebuilding it from
    /// `base_url` alone would 401 against any key-protected server. `None` on
    /// the [`Self::with_transport`] path, whose auth lives in the transport's
    /// headers rather than an `Endpoint`.
    endpoint: Option<Endpoint>,
}

impl OpenAiChatClient {
    /// Build from the `OPENAI_*` environment variables (the default endpoint
    /// when no named endpoint from config is selected).
    pub fn from_env() -> Result<Self, DomainError> {
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
        Self::from_config_with_model(data_dir, endpoint_override, None)
    }

    /// Like [`Self::from_config`] but applies a model override on top of the
    /// resolved endpoint's own — the path a per-usage binding takes when it
    /// names only a model and keeps the endpoint.
    pub fn from_config_with_model(
        data_dir: &str,
        endpoint_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self, DomainError> {
        let cfg = super::CodesearchConfig::load(data_dir)?;
        match cfg.resolve_openai_endpoint(endpoint_override) {
            Some(ep) => {
                let model = model_override
                    .or(ep.model.as_deref())
                    .unwrap_or(DEFAULT_MODEL);
                Self::from_endpoint(
                    &ep.base_url,
                    model,
                    ep.api_key.as_deref(),
                    DEFAULT_TIMEOUT_SECS,
                )
            }
            // Nothing registered: fall back to the environment, still honouring
            // a model the usage named.
            None => match model_override {
                Some(model) => {
                    let base = std::env::var("OPENAI_BASE_URL")
                        .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
                    let api_key = std::env::var("OPENAI_API_KEY")
                        .ok()
                        .filter(|k| !k.is_empty());
                    Self::from_endpoint(&base, model, api_key.as_deref(), DEFAULT_TIMEOUT_SECS)
                }
                None => Self::from_env(),
            },
        }
    }

    /// Build from an explicit endpoint (a named endpoint from config): `base`
    /// URL, `model`, and optional bearer `api_key`.
    pub fn from_endpoint(
        base: &str,
        model: &str,
        api_key: Option<&str>,
        timeout_secs: u64,
    ) -> Result<Self, DomainError> {
        let base_url = base.trim_end_matches('/').to_string();
        debug!("OpenAiChatClient: base={base_url}, model={model}");
        let endpoint = Endpoint::new(base_url.clone())
            .with_optional_api_key(api_key.filter(|k| !k.is_empty()))
            .with_timeout(Duration::from_secs(timeout_secs));
        let inner = CrateChatClient::new(&endpoint, model).map_err(super::map_openai_err)?;
        Ok(Self {
            inner,
            base_url,
            endpoint: Some(endpoint),
        })
    }

    /// Construct from a pre-built [`Transport`] (whose headers already carry any
    /// auth) plus the `model` to send. Used by
    /// [`CopilotChatClient`](super::CopilotChatClient), which speaks the same
    /// OpenAI-compatible protocol against the Copilot API — so it reuses all of
    /// this client's request/stream logic instead of duplicating it.
    pub fn with_transport(transport: Transport, model: String) -> Self {
        let base_url = transport.base_url().to_string();
        let inner = CrateChatClient::with_transport(transport, model);
        Self {
            inner,
            base_url,
            endpoint: None,
        }
    }

    /// The base URL this client is configured to use — useful for log messages.
    pub fn configured_base_url(&self) -> String {
        self.base_url.clone()
    }

    /// The model id this client sends in chat requests.
    pub fn configured_model(&self) -> &str {
        self.inner.model()
    }

    /// Discover the models the server offers via `GET /v1/models`.
    ///
    /// Returns their ids (e.g. `"google/gemma-4-e2b"`). Works against any
    /// OpenAI-compatible server (LM Studio, OpenAI, vLLM, …). Errors if the
    /// endpoint is unreachable or returns a non-success status.
    pub async fn list_models(&self) -> Result<Vec<String>, DomainError> {
        // Reuse the configured endpoint so the catalog call carries the same api
        // key and timeout as chat requests; only the transport-built path (whose
        // auth lives in its headers) falls back to a bare endpoint.
        let fallback;
        let endpoint = match &self.endpoint {
            Some(ep) => ep,
            None => {
                fallback = Endpoint::new(self.base_url.clone());
                &fallback
            }
        };
        let catalog = OpenAiModelCatalog::new(endpoint).map_err(super::map_openai_err)?;
        let models = catalog.list_models().await.map_err(super::map_openai_err)?;
        Ok(models.into_iter().map(|m| m.id).collect())
    }

    /// Build a codesearch [`ChatRequest`](openai_rs::ChatRequest) for the given
    /// prompt with codesearch's deterministic temperature pinned.
    fn request(&self, system: &str, user: &str) -> ChatRequest {
        ChatRequest::from_prompt(system, user).with_temperature(DETERMINISTIC_TEMPERATURE)
    }
}

#[async_trait]
impl ChatClient for OpenAiChatClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, DomainError> {
        self.inner
            .chat(&self.request(system, user))
            .await
            .map_err(super::map_openai_err)
    }

    async fn complete_json(
        &self,
        system: &str,
        user: &str,
        schema_name: &str,
        schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        let request = self
            .request(system, user)
            .with_schema(JsonSchema::new(schema_name, schema.clone()));
        self.inner
            .chat(&request)
            .await
            .map_err(super::map_openai_err)
    }

    async fn complete_stream(
        &self,
        system: &str,
        user: &str,
        token_tx: UnboundedSender<String>,
    ) -> Result<String, DomainError> {
        self.inner
            .chat_stream(&self.request(system, user), token_tx)
            .await
            .map_err(super::map_openai_err)
    }
}
