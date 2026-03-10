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
        is_regex: bool,
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
            .execute(&symbol, repository.as_deref(), chat_client.as_ref(), is_regex)
            .await
            .context("Explain use case failed")?;

        if !result.ambiguous_candidates.is_empty() {
            let mut output = format!(
                "'{}' matches {} symbols — please pick one and re-run with the full name:\n\n",
                result.root_symbol,
                result.ambiguous_candidates.len(),
            );
            for (i, candidate) in result.ambiguous_candidates.iter().enumerate() {
                output.push_str(&format!("  {}. {}\n", i + 1, candidate));
            }
            if result.is_regex {
                output.push_str(
                    "\nTip: narrow or anchor your regex to match a single symbol, \
                     e.g. use `^pattern` or `pattern$`.\n",
                );
            } else {
                output.push_str(
                    "\nRun with the full symbol name to explain a specific one.\n",
                );
            }
            return Ok(output);
        }

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
