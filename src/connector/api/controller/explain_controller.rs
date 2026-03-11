use std::io::Write as _;
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

        let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Run the use case in a separate task so we can concurrently drain the
        // token channel and print tokens to stdout as they arrive.
        let use_case = self.container.explain_use_case();
        let symbol_c = symbol.clone();
        let repo_c = repository.clone();
        let client_c = chat_client.clone();

        let use_case_handle = tokio::spawn(async move {
            use_case
                .execute_streaming(
                    &symbol_c,
                    repo_c.as_deref(),
                    client_c.as_ref(),
                    is_regex,
                    token_tx,
                )
                .await
        });

        // Stream tokens to stdout as they arrive.  The channel is closed
        // (recv returns None) when the use case task drops its sender, which
        // happens naturally once complete_stream returns.
        while let Some(token) = token_rx.recv().await {
            print!("{token}");
            std::io::stdout().flush().ok();
        }

        let result: crate::application::ExplainResult = use_case_handle
            .await
            .context("explain task panicked")?
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
                output.push_str("\nRun with the full symbol name to explain a specific one.\n");
            }
            return Ok(output);
        }

        // The LLM tokens have already been printed to stdout by the loop above.
        // Build the trailing section: stats + optional source dump + file list.
        let mut trailing = format!(
            "\n\n---\nAnalysed {} symbols across {} call levels.\n\n",
            result.total_affected,
            result.max_depth_reached,
        );

        if dump_symbols {
            for (symbol, repository, file_path, line, src) in &result.symbol_sources {
                match src {
                    Some(s) => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}`\n```\n{}\n```\n\n",
                        symbol, repository, file_path, line, s
                    )),
                    None => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}` _(source not available)_\n\n",
                        symbol, repository, file_path, line
                    )),
                }
            }
        }

        if !result.symbol_sources.is_empty() {
            trailing.push_str("## Referenced files\n\n");
            for (symbol, repository, file_path, line, _src) in &result.symbol_sources {
                trailing.push_str(&format!(
                    "- `{}` `{}:{}` — `{}`\n",
                    repository, file_path, line, symbol
                ));
            }
            trailing.push('\n');
        }

        Ok(trailing)
    }
}
