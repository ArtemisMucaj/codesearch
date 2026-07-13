//! LLM backend introspection for serve mode.
//!
//! - `GET /api/llm/models` — the chat models available to the active LLM
//!   backend, so a native-app / web client can populate a model picker and then
//!   pass a chosen `model` back to the streaming endpoints on the fly.
//!
//! The backend defaults to the one the server was started with
//! (`--llm-target`), and can be overridden per request with `?target=`
//! (`openai` | `anthropic` | `copilot`).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cli::LlmTarget;
use crate::connector::adapter::{CopilotChatClient, OpenAiChatClient};

use super::super::error::{ApiError, ApiResult};
use super::super::server::AppState;

/// Query params for `GET /api/llm/models`.
#[derive(Debug, Default, Deserialize)]
pub struct LlmModelsParams {
    /// Override the backend to query: `openai`, `anthropic`, or `copilot`.
    /// Omit to use the server's configured `--llm-target`.
    #[serde(default)]
    pub target: Option<String>,
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
            let client = OpenAiChatClient::from_env().map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to initialise OpenAI client: {e}"),
                )
            })?;
            client
                .list_models()
                .await?
                .into_iter()
                .map(|id| ModelInfo { id, name: None })
                .collect()
        }
        LlmTarget::Copilot => {
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
