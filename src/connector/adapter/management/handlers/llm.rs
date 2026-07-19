//! LLM backend introspection and configuration for serve mode.
//!
//! - `GET /api/llm/models` — the chat models available to the active LLM
//!   backend, so a native-app / web client can populate a model picker and then
//!   pass a chosen `model` back to the streaming endpoints on the fly.
//! - `GET /api/llm/endpoints` — list the configured OpenAI-compatible endpoints
//!   and which is active (keys masked).
//! - `PUT /api/llm/endpoints/{name}` — add or update an endpoint.
//! - `POST /api/llm/active` — set the active endpoint.
//!
//! Together these let a running server (and a native app talking to it)
//! configure LLM backends at runtime, not just via the CLI. Endpoint changes
//! persist to `<data_dir>/config.json` (mode `0600`).
//!
//! The backend for `models` defaults to the one the server was started with
//! (`--llm-target`), and can be overridden per request with `?target=`
//! (`openai` | `anthropic` | `copilot`).

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cli::LlmTarget;
use crate::connector::adapter::{
    CodesearchConfig, CopilotChatClient, OpenAiChatClient, OpenAiEndpoint,
};

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Query params for `GET /api/llm/models`.
#[derive(Debug, Default, Deserialize)]
pub struct LlmModelsParams {
    /// Override the backend to query: `openai`, `anthropic`, or `copilot`.
    /// Omit to use the server's configured `--llm-target`.
    #[serde(default)]
    pub target: Option<String>,
    /// For the OpenAI backend: which named endpoint from config to query. Omit
    /// to use the configured `active` endpoint (then `OPENAI_*`).
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// A single model in the response.
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    /// The id to pass back as `model` on the streaming endpoints.
    pub id: String,
    /// Human-readable name when the backend provides one (Copilot does; the
    /// OpenAI `/v1/models` list only carries ids).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Response body for `GET /api/llm/models`.
#[derive(Debug, Serialize)]
pub struct LlmModelsResponse {
    /// The backend the models belong to (`openai` | `anthropic` | `copilot`).
    pub target: String,
    /// Available models, most-relevant order as returned by the backend.
    pub models: Vec<ModelInfo>,
}

/// `GET /api/llm/models` — list the chat models available to the active (or
/// requested) LLM backend.
pub async fn models(
    State(state): State<AppState>,
    Query(params): Query<LlmModelsParams>,
) -> ApiResult<Json<LlmModelsResponse>> {
    let target = match params.target.as_deref() {
        None => state.container.llm_target(),
        Some(t) => t.parse::<LlmTarget>().map_err(ApiError::bad_request)?,
    };

    let models = match target {
        LlmTarget::OpenAi => {
            let client = OpenAiChatClient::from_config(
                state.container.data_dir(),
                params.endpoint.as_deref(),
            )?;
            client
                .list_models()
                .await?
                .into_iter()
                .map(|id| ModelInfo { id, name: None })
                .collect()
        }
        LlmTarget::Copilot => {
            // Without a stored OAuth token every Copilot request 401s, which
            // would surface as an opaque 500. Detect it up front and return a
            // clear, actionable 400 instead.
            let copilot = CodesearchConfig::load_copilot(state.container.data_dir())?;
            if copilot.github_token.as_deref().unwrap_or("").is_empty() {
                return Err(ApiError::bad_request(
                    "GitHub Copilot is not authenticated — run `codesearch copilot login`",
                ));
            }
            let client = CopilotChatClient::from_data_dir(state.container.data_dir())?;
            client
                .list_models()
                .await?
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.id,
                    name: Some(m.name),
                })
                .collect()
        }
        LlmTarget::Anthropic => {
            // The Anthropic Messages API has no model-discovery endpoint that is
            // uniformly available across compatible servers (LM Studio, the
            // Anthropic cloud, …), so we don't invent one. Point at OpenAI or
            // Copilot for discovery.
            return Err(ApiError::bad_request(
                "model discovery is not available for the anthropic backend; \
                 use ?target=openai or ?target=copilot",
            ));
        }
    };

    Ok(Json(LlmModelsResponse {
        target: target.as_str().to_string(),
        models,
    }))
}

// ---------------------------------------------------------------------------
// OpenAI-compatible endpoint management
// ---------------------------------------------------------------------------

/// One endpoint in the `GET /api/llm/endpoints` response. The API key is never
/// returned; only whether one is set (`has_key`).
#[derive(Debug, Serialize)]
pub struct EndpointInfo {
    pub name: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub has_key: bool,
    /// Whether this is the configured `active` endpoint.
    pub active: bool,
}

/// Response body for `GET /api/llm/endpoints`.
#[derive(Debug, Serialize)]
pub struct EndpointsResponse {
    pub active: Option<String>,
    pub endpoints: Vec<EndpointInfo>,
}

/// `GET /api/llm/endpoints` — list configured OpenAI-compatible endpoints. API
/// keys are masked (`has_key`), so this is safe to expose over the management
/// API.
pub async fn list_endpoints(State(state): State<AppState>) -> ApiResult<Json<EndpointsResponse>> {
    let cfg = CodesearchConfig::load_async(state.container.data_dir()).await?;
    let openai = cfg.openai.unwrap_or_default();
    let active = openai.active.clone();

    let endpoints = openai
        .endpoints
        .into_iter()
        .map(|(name, ep)| EndpointInfo {
            active: active.as_deref() == Some(name.as_str()),
            has_key: ep.api_key.as_deref().is_some_and(|k| !k.is_empty()),
            name,
            base_url: ep.base_url,
            model: ep.model,
        })
        .collect();

    Ok(Json(EndpointsResponse { active, endpoints }))
}

/// Request body for `PUT /api/llm/endpoints/{name}`.
#[derive(Debug, Deserialize)]
pub struct UpsertEndpointRequest {
    pub base_url: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Bearer API key. Write-only: it is stored but never returned by any GET.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Make this the active endpoint after saving.
    #[serde(default)]
    pub set_active: bool,
}

/// `PUT /api/llm/endpoints/{name}` — add or update an endpoint.
pub async fn upsert_endpoint(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpsertEndpointRequest>,
) -> ApiResult<Json<EndpointsResponse>> {
    if name.trim().is_empty() {
        return Err(ApiError::bad_request("endpoint name must not be empty"));
    }
    let data_dir = state.container.data_dir().to_string();
    let mut cfg = CodesearchConfig::load_async(&data_dir).await?;
    let openai = cfg.openai_mut();
    openai.endpoints.insert(
        name.clone(),
        OpenAiEndpoint {
            base_url: body.base_url,
            model: body.model,
            api_key: body.api_key.filter(|k| !k.is_empty()),
        },
    );
    // Activate on explicit request, or when this is the first endpoint — matching
    // the `codesearch openai add` CLI so both paths behave the same.
    if body.set_active || openai.active.is_none() {
        openai.active = Some(name);
    }
    cfg.save_async(&data_dir).await?;

    list_endpoints(State(state)).await
}

/// Request body for `POST /api/llm/active`.
#[derive(Debug, Deserialize)]
pub struct SetActiveRequest {
    pub name: String,
}

/// `POST /api/llm/active` — set the active OpenAI endpoint.
pub async fn set_active_endpoint(
    State(state): State<AppState>,
    Json(body): Json<SetActiveRequest>,
) -> ApiResult<Json<EndpointsResponse>> {
    let data_dir = state.container.data_dir().to_string();
    let mut cfg = CodesearchConfig::load_async(&data_dir).await?;
    let openai = cfg.openai_mut();
    if !openai.endpoints.contains_key(&body.name) {
        return Err(ApiError::not_found(format!(
            "no endpoint named '{}'",
            body.name
        )));
    }
    openai.active = Some(body.name);
    cfg.save_async(&data_dir).await?;

    list_endpoints(State(state)).await
}
