use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::application::CallGraphQuery;
use crate::cli::LlmTarget;
use crate::connector::adapter::{AnthropicClient, ChatClient, OpenAiChatClient};
use crate::ImpactNode;

use super::super::Container;

/// Maximum lines to include per source window around a reference.
const SOURCE_WINDOW_LINES: usize = 40;

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
            let callees = match call_graph
                .find_callees(&analysis.root_symbol, &cg_query)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        error = %e,
                        symbol = %analysis.root_symbol,
                        "Failed to find callees for root symbol; root source will be unavailable"
                    );
                    Vec::new()
                }
            };
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

/// Reconstruct all call paths from the BFS result.
///
/// Each path is ordered outermost-caller-first: `path[0]` is the most
/// distant symbol from the root (the entry point), `path[last]` is the
/// direct caller of the root symbol. The root itself is not included —
/// it is appended by the caller when rendering the chain header.
fn reconstruct_paths<'a>(by_depth: &'a [Vec<ImpactNode>]) -> Vec<Vec<&'a ImpactNode>> {
    let all_nodes: Vec<&ImpactNode> = by_depth.iter().flatten().collect();

    // children_map[sym] = nodes that list sym as their via_symbol (callers of sym).
    let mut children_map: HashMap<&str, Vec<&ImpactNode>> = HashMap::new();
    for node in &all_nodes {
        if let Some(via) = node.via_symbol.as_deref() {
            children_map.entry(via).or_default().push(node);
        }
    }

    // Leaf nodes: no other node calls through them (outermost callers).
    let leaf_nodes: Vec<&ImpactNode> = all_nodes
        .iter()
        .copied()
        .filter(|n| !children_map.contains_key(n.symbol.as_str()))
        .collect();

    // Lookup for unambiguous path tracing: (depth, symbol, repository_id) → node.
    // Including repository_id prevents collisions when the same symbol name exists
    // at the same depth in multiple repositories.
    let mut node_by_depth_symbol: HashMap<(usize, &str, &str), &ImpactNode> = HashMap::new();
    for node in &all_nodes {
        node_by_depth_symbol
            .entry((node.depth, node.symbol.as_str(), node.repository_id.as_str()))
            .or_insert(node);
    }

    let mut paths = Vec::new();
    for leaf in leaf_nodes {
        // Trace from leaf back toward the root symbol.
        let mut path = vec![leaf];
        let mut current = leaf;
        while let Some(via) = current.via_symbol.as_deref() {
            let parent_depth = current.depth.saturating_sub(1);
            if let Some(&parent) = node_by_depth_symbol.get(&(parent_depth, via, current.repository_id.as_str())) {
                path.push(parent);
                current = parent;
            } else {
                break;
            }
        }
        // path[0] = outermost caller (leaf), path[last] = direct caller of root.
        paths.push(path);
    }
    paths
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

    if by_depth.is_empty() {
        return prompt;
    }

    let paths = reconstruct_paths(by_depth);
    let total_paths = paths.len();

    // Collect unique nodes to read source for (dedup by (symbol, file_path) so that
    // the same symbol defined in different files each gets its own source read).
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut nodes_to_read: Vec<&ImpactNode> = Vec::new();
    for path in &paths {
        for node in path {
            if seen.insert((node.symbol.as_str(), node.file_path.as_str())) {
                nodes_to_read.push(node);
            }
        }
    }

    // Read sources sequentially (cannot await inside a closure).
    let mut source_cache: HashMap<(&str, &str), Option<String>> = HashMap::new();
    for node in nodes_to_read {
        let src = read_source_window(&node.file_path, node.line, SOURCE_WINDOW_LINES).await;
        source_cache.insert((node.symbol.as_str(), node.file_path.as_str()), src);
    }

    prompt.push_str(&format!("## Call paths ({total_paths} total)\n\n"));

    for (i, path) in paths.iter().enumerate() {
        // Chain header reads outermost → ... → direct_caller → root_symbol.
        let chain: String = path
            .iter()
            .map(|n| n.symbol.as_str())
            .chain(std::iter::once(root_symbol))
            .collect::<Vec<_>>()
            .join(" → ");
        prompt.push_str(&format!("### Path {} — `{}`\n\n", i + 1, chain));

        // Render each node in the path (outermost first).
        for node in path {
            let src_block = match source_cache.get(&(node.symbol.as_str(), node.file_path.as_str())) {
                Some(Some(src)) => format!("```\n{src}\n```"),
                _ => "_(source not available)_".to_string(),
            };
            prompt.push_str(&format!(
                "#### `{}` — `{}:{}`\n{}\n\n",
                node.symbol, node.file_path, node.line, src_block
            ));
        }
    }

    prompt
}

/// Read `window` lines of source from `file_path` centred on `center_line` (1-indexed).
/// Returns `None` if the file cannot be read.
async fn read_source_window(file_path: &str, center_line: u32, window: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(file_path).await.ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    // Convert to 0-indexed and clamp so center is always a valid index.
    let center = (center_line.saturating_sub(1) as usize).min(lines.len().saturating_sub(1));
    let half = window / 2;
    let start = center.saturating_sub(half);
    let end = (start + window).min(lines.len());
    if start >= end {
        return None;
    }
    Some(lines[start..end].join("\n"))
}
