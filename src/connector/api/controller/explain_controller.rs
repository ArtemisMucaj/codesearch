use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::debug;

use crate::application::CallGraphQuery;
use crate::cli::LlmTarget;
use crate::connector::adapter::{AnthropicClient, ChatClient, OpenAiChatClient};
use crate::ImpactNode;

use super::super::Container;

/// Maximum lines to include per source window around a reference.
const SOURCE_WINDOW_LINES: usize = 40;

/// Maximum number of depth-1 callers for which full source is included in the prompt.
/// Beyond this limit only the symbol name and location are listed to stay within
/// the model's context budget.
const MAX_CALLERS_WITH_SOURCE: usize = 5;

const SYSTEM_PROMPT: &str = "\
You are a senior software engineer performing call-flow analysis. \
Given a symbol's source code and its call graph, write a clear and precise explanation covering:

1. **Purpose** — what the root symbol does and why it exists.
2. **Data / control flow** — how data enters, transforms, and exits through each call level.
3. **Business feature** — the user-visible capability or requirement this code implements.
4. **Key patterns & dependencies** — notable abstractions, external services, or design patterns used.

Be concrete: reference specific function names, file paths, and argument names where helpful. \
Format your response with Markdown headings.";

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
    ) -> Result<String> {
        // Build the chat client requested by the caller.
        let chat_client: Arc<dyn ChatClient> = match llm {
            LlmTarget::Anthropic => Arc::new(AnthropicClient::from_env()),
            LlmTarget::OpenAi => Arc::new(
                OpenAiChatClient::from_env()
                    .context("Failed to initialise OpenAI chat client for explain command")?,
            ),
        };

        // Run impact analysis to obtain the full call graph for this symbol.
        let impact_uc = self.container.impact_use_case();
        let analysis = impact_uc
            .analyze(&symbol, repository.as_deref())
            .await?;

        if analysis.total_affected == 0 {
            return Ok(format!(
                "No callers found for '{}'. \
                 The symbol may be a root entry point or has not been indexed yet.",
                symbol
            ));
        }

        // Locate the root symbol's definition file via its callees (what it calls).
        let call_graph = self.container.call_graph_use_case();
        let cg_query = match repository.as_deref() {
            Some(repo_id) => CallGraphQuery::new().with_repository(repo_id),
            None => CallGraphQuery::new(),
        };

        let root_source = {
            let callees = call_graph
                .find_callees(&analysis.root_symbol, &cg_query)
                .await
                .unwrap_or_default();
            match callees.first() {
                Some(ref_) => {
                    read_source_window(ref_.caller_file_path(), ref_.reference_line(), SOURCE_WINDOW_LINES)
                        .await
                        .map(|src| (ref_.caller_file_path().to_string(), src))
                }
                None => None,
            }
        };

        // Build the user prompt.
        let prompt = build_prompt(&analysis.root_symbol, root_source, &analysis.by_depth).await;

        debug!(
            symbol = %analysis.root_symbol,
            prompt_chars = prompt.len(),
            "Sending explain prompt to LLM"
        );

        let explanation = chat_client
            .complete(SYSTEM_PROMPT, &prompt)
            .await
            .context("LLM call failed during explain command")?;

        Ok(format!(
            "Explanation for `{}`\n{}\n\n{}\n\n---\nAnalysed {} symbols across {} call levels.\n",
            analysis.root_symbol,
            "═".repeat(60),
            explanation,
            analysis.total_affected,
            analysis.max_depth_reached,
        ))
    }
}

/// Construct the structured user prompt from the impact graph.
async fn build_prompt(
    root_symbol: &str,
    root_source: Option<(String, String)>,
    by_depth: &[Vec<ImpactNode>],
) -> String {
    let mut prompt = format!("# Call-flow explanation request: `{root_symbol}`\n\n");

    // Root symbol source (definition context).
    match root_source {
        Some((file_path, src)) => {
            prompt.push_str(&format!(
                "## Root symbol — `{root_symbol}`\n\
                 Source from `{file_path}`:\n\
                 ```\n{src}\n```\n\n"
            ));
        }
        None => {
            prompt.push_str(&format!(
                "## Root symbol — `{root_symbol}`\n\
                 _(source not available — symbol may not call any other indexed symbol)_\n\n"
            ));
        }
    }

    // Depth-1 callers: include full source for the first MAX_CALLERS_WITH_SOURCE.
    if let Some(depth1) = by_depth.first() {
        if !depth1.is_empty() {
            prompt.push_str("## Direct callers (depth 1)\n\n");
            for (i, node) in depth1.iter().enumerate() {
                if i < MAX_CALLERS_WITH_SOURCE {
                    prompt.push_str(&format_node_with_source(i + 1, node).await);
                } else {
                    // Remaining callers without source to save context budget.
                    prompt.push_str(&format!(
                        "- `{}` — `{}:{}`\n",
                        node.symbol, node.file_path, node.line
                    ));
                }
            }
            prompt.push('\n');
        }
    }

    // Deeper depths: summary only (no source).
    for (depth_idx, nodes) in by_depth.iter().enumerate().skip(1) {
        if nodes.is_empty() {
            continue;
        }
        prompt.push_str(&format!("## Depth {} callers\n\n", depth_idx + 1));
        for node in nodes {
            let via = node.via_symbol.as_deref().unwrap_or("(unknown)");
            prompt.push_str(&format!(
                "- `{}` via `{}` — `{}:{}`\n",
                node.symbol, via, node.file_path, node.line
            ));
        }
        prompt.push('\n');
    }

    prompt
}

/// Format a single impact node with its source window (if readable).
async fn format_node_with_source(index: usize, node: &ImpactNode) -> String {
    let header = format!(
        "### {}. `{}` — `{}:{}`\n",
        index, node.symbol, node.file_path, node.line
    );
    match read_source_window(&node.file_path, node.line, SOURCE_WINDOW_LINES).await {
        Some(src) => format!("{header}```\n{src}\n```\n\n"),
        None => format!("{header}_(source not available)_\n\n"),
    }
}

/// Read `window` lines of source from `file_path` centred on `center_line` (1-indexed).
/// Returns `None` if the file cannot be read.
async fn read_source_window(file_path: &str, center_line: u32, window: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(file_path).await.ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let center = center_line.saturating_sub(1) as usize; // convert to 0-indexed
    let half = window / 2;
    let start = center.saturating_sub(half);
    let end = (start + window).min(lines.len());
    Some(lines[start..end].join("\n"))
}
