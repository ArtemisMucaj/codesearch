use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::AsyncWriteExt as _;

use anyhow::{Context, Result};

use crate::application::ChatClient;
use crate::cli::LlmTarget;

use super::super::Container;
use super::build_chat_client_for;
use crate::connector::adapter::LlmUsage;

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
        let chat_client: Arc<dyn ChatClient> =
            build_chat_client_for(LlmUsage::ExplainCode, llm, self.container.data_dir())?;

        let use_case = self.container.explain_use_case();

        // The explanation is generated in full before anything is printed, so
        // let the user know the LLM call is under way. Progress goes to stderr
        // to keep stdout a clean, pipeable Markdown document.
        eprintln!("Analysing `{symbol}` …");

        let result: crate::application::ExplainResult = use_case
            .execute(
                &symbol,
                repository.as_deref(),
                chat_client.as_ref(),
                is_regex,
            )
            .await
            .context("Explain use case failed")?;

        let mut out = tokio::io::stdout();

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

        // Resolve repository IDs to human-readable names for the trailing section.
        let metadata_repo = self.container.metadata_repository();
        let mut repo_name_cache: HashMap<String, String> = HashMap::new();
        for (_symbol, repo_id, _file_path, _line, _src) in &result.symbol_sources {
            if !repo_name_cache.contains_key(repo_id.as_str()) {
                let name = metadata_repo
                    .find_by_id(repo_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.name().to_string())
                    .unwrap_or_else(|| repo_id.clone());
                repo_name_cache.insert(repo_id.clone(), name);
            }
        }

        // Build and write the explanation plus its trailing section directly to
        // stdout so that main.rs's println! does not duplicate the output.
        let mut trailing = format!(
            "{}\n\n---\nAnalysed {} symbols across {} call levels.\n\n",
            result.explanation, result.total_affected, result.max_depth_reached,
        );

        if dump_symbols {
            for (symbol, repo_id, file_path, line, src) in &result.symbol_sources {
                let repo_name = repo_name_cache
                    .get(repo_id.as_str())
                    .map(String::as_str)
                    .unwrap_or(repo_id.as_str());
                match src {
                    Some(s) => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}`\n```\n{}\n```\n\n",
                        symbol, repo_name, file_path, line, s
                    )),
                    None => trailing.push_str(&format!(
                        "`{}` (`{}`) — `{}:{}` _(source not available)_\n\n",
                        symbol, repo_name, file_path, line
                    )),
                }
            }
        }

        if !result.symbol_sources.is_empty() {
            trailing.push_str("## Referenced files\n\n");
            for (symbol, repo_id, file_path, line, _src) in &result.symbol_sources {
                let repo_name = repo_name_cache
                    .get(repo_id.as_str())
                    .map(String::as_str)
                    .unwrap_or(repo_id.as_str());
                trailing.push_str(&format!(
                    "- {} {}:{} — {}\n",
                    repo_name, file_path, line, symbol
                ));
            }
            trailing.push('\n');
        }

        out.write_all(trailing.as_bytes())
            .await
            .context("failed to write trailing section to stdout")?;
        out.flush().await.context("failed to flush stdout")?;

        // Everything has been written to stdout directly; return empty so
        // main.rs's println! does not print a duplicate.
        Ok(String::new())
    }
}
