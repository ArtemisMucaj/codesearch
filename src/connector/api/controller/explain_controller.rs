use std::sync::Arc;

use anyhow::{Context, Result};

use crate::application::ChatClient;
use crate::cli::LlmTarget;
use crate::connector::adapter::{AnthropicClient, OpenAiChatClient};

use super::super::Container;

pub struct ExplainController<'a> {
    container: &'a Container,
}

impl<'a> ExplainController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn explain(
        &self,
        symbol: String,
        repository: Option<String>,
        llm: LlmTarget,
        dump_symbols: bool,
    ) -> Result<String> {
        let chat_client: Arc<dyn ChatClient> = match llm {
            LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
            LlmTarget::OpenAi => Arc::new(
                OpenAiChatClient::from_env()
                    .context("Failed to initialise OpenAI chat client for explain command")?,
            ),
        };

        let result: crate::application::ExplainResult = self
            .container
            .explain_use_case()
            .execute(&symbol, repository.as_deref(), chat_client.as_ref())
            .await
            .context("Explain use case failed")?;

        let mut output = format!(
            "Explanation for `{}`\n{}\n\n{}\n\n---\nAnalysed {} symbols across {} call levels.\n\n",
            result.root_symbol,
            "═".repeat(60),
            result.explanation,
            result.total_affected,
            result.max_depth_reached,
        );

        if dump_symbols {
            for (symbol, file_path, line, src) in &result.symbol_sources {
                match src {
                    Some(s) => output.push_str(&format!(
                        "**`{}`** — `{}:{}`\n```\n{}\n```\n\n",
                        symbol, file_path, line, s
                    )),
                    None => output.push_str(&format!(
                        "**`{}`** — `{}:{}` _(source not available)_\n\n",
                        symbol, file_path, line
                    )),
                }
            }
        }

        Ok(output)
    }
}
