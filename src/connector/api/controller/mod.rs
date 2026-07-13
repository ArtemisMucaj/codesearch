use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::{AnthropicClient, CopilotChatClient, OpenAiChatClient};

/// Build a chat client for the requested provider. The Anthropic/OpenAI
/// backends read their endpoint config from the environment (`ANTHROPIC_*` /
/// `OPENAI_*`); the Copilot backend reads its token and model from
/// `<data_dir>/config.json`. Shared by every controller that needs an LLM
/// (explain, memory, community naming) so provider dispatch lives in one place.
pub(crate) fn build_chat_client(llm: LlmTarget, data_dir: &str) -> Result<Arc<dyn ChatClient>> {
    Ok(match llm {
        LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
        LlmTarget::OpenAi => Arc::new(
            OpenAiChatClient::from_env().context("Failed to initialise OpenAI chat client")?,
        ),
        LlmTarget::Copilot => Arc::new(
            CopilotChatClient::from_data_dir(data_dir)
                .context("Failed to initialise Copilot chat client")?,
        ),
    })
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
pub mod memory_controller;
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
pub use memory_controller::{run_import_picker_ui, MemoryController};
pub use search_controller::SearchController;
pub use stats_controller::StatsController;
pub use symbol_clusters_controller::SymbolClustersController;
pub use symbol_context_controller::SymbolContextController;
pub use uses_controller::UsesController;
pub use visualize_controller::VisualizeController;
