use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::{
    AnthropicClient, CodesearchConfig, CopilotChatClient, LlmUsage, OpenAiChatClient,
    COPILOT_ENDPOINT,
};

/// Build a chat client for the requested provider. The Anthropic backend reads
/// its endpoint from the environment (`ANTHROPIC_*`); the OpenAI backend resolves
/// a named endpoint from `<data_dir>/config.json` (the configured `active` one)
/// and falls back to `OPENAI_*`; the Copilot backend reads its token and model
/// from config. Shared by every controller that needs an LLM (explain,
/// community naming) so provider dispatch lives in one place.
pub(crate) fn build_chat_client(llm: LlmTarget, data_dir: &str) -> Result<Arc<dyn ChatClient>> {
    Ok(match llm {
        LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
        LlmTarget::OpenAi => Arc::new(
            OpenAiChatClient::from_config(data_dir, None)
                .context("Failed to initialise OpenAI chat client")?,
        ),
        LlmTarget::Copilot => Arc::new(
            CopilotChatClient::from_data_dir(data_dir)
                .context("Failed to initialise Copilot chat client")?,
        ),
    })
}

/// Build the chat client for one **usage**, honouring its per-usage override and
/// otherwise falling back to the active backend.
///
/// Read from disk per call rather than resolved once, so a change made through
/// the management API applies to the next request without restarting `serve`.
pub(crate) fn build_chat_client_for(
    usage: LlmUsage,
    llm: LlmTarget,
    data_dir: &str,
) -> Result<Arc<dyn ChatClient>> {
    let cfg = CodesearchConfig::load(data_dir).unwrap_or_default();
    let binding = cfg.usages.get(usage.as_str()).cloned().unwrap_or_default();

    // The reserved `copilot` name selects the Copilot backend regardless of the
    // active target, so a single usage can differ from everything else.
    if binding.endpoint.as_deref() == Some(COPILOT_ENDPOINT) {
        let client = CopilotChatClient::from_data_dir_with_model(data_dir, binding.model.clone())
            .context("Failed to initialise Copilot chat client")?;
        return Ok(Arc::new(client));
    }

    // No override at all: exactly the previous behaviour.
    if binding.endpoint.is_none() && binding.model.is_none() {
        return build_chat_client(llm, data_dir);
    }

    // An override implies an OpenAI-compatible endpoint (Copilot is handled
    // above, and Anthropic has no named-endpoint registry to select from).
    let client = OpenAiChatClient::from_config_with_model(
        data_dir,
        binding.endpoint.as_deref(),
        binding.model.as_deref(),
    )
    .context("Failed to initialise OpenAI chat client")?;
    Ok(Arc::new(client))
}

pub mod channels_controller;
pub mod clusters_controller;
pub mod couplings_controller;
pub mod delete_controller;
pub mod execution_features_controller;
pub mod explain_controller;
pub mod impact_controller;
pub mod index_controller;
pub mod list_repositories_controller;
pub mod overview_controller;
pub mod search_controller;
pub mod stats_controller;
pub mod symbol_clusters_controller;
pub mod symbol_context_controller;
pub mod uses_controller;
pub mod visualize_controller;

pub use channels_controller::ChannelsController;
pub use clusters_controller::ClustersController;
pub use couplings_controller::CouplingsController;
pub use delete_controller::DeleteController;
pub use execution_features_controller::ExecutionFeaturesController;
pub use explain_controller::ExplainController;
pub use impact_controller::ImpactController;
pub use index_controller::IndexController;
pub use list_repositories_controller::ListRepositoriesController;
pub use overview_controller::OverviewController;
pub use search_controller::SearchController;
pub use stats_controller::StatsController;
pub use symbol_clusters_controller::SymbolClustersController;
pub use symbol_context_controller::SymbolContextController;
pub use uses_controller::UsesController;
pub use visualize_controller::VisualizeController;
