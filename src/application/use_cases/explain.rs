use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::warn;

use crate::application::{CallGraphQuery, CallGraphUseCase, ChatClient};
use crate::domain::DomainError;

use super::impact_analysis::{ImpactAnalysisUseCase, ImpactNode};
use super::snippet_lookup::SnippetLookupUseCase;

const SYSTEM_PROMPT: &str = "\
You are a senior software engineer performing call-flow analysis. \
Given a symbol's source code and its call graph, write a clear and precise explanation covering:

1. **Purpose** — what the root symbol does and why it exists.
2. **Data / control flow** — how data enters, transforms, and exits through each call level.
3. **Business feature** — the user-visible capability or requirement this code implements.
4. **Key patterns & dependencies** — notable abstractions, external services, or design patterns used.

Be concrete: reference specific function names, file paths, and argument names where helpful. \
Format your response with Markdown headings.";

/// Output produced by [`ExplainUseCase::execute`].
pub struct ExplainResult {
    pub root_symbol: String,
    pub explanation: String,
    pub total_affected: usize,
    pub max_depth_reached: usize,
    /// Unique symbols whose source chunks were sent to the LLM.
    /// Each entry is `(symbol, file_path, line, source)`.
    pub symbol_sources: Vec<(String, String, u32, Option<String>)>,
}

/// Orchestrates impact analysis, call-graph traversal, snippet retrieval,
/// prompt construction, and LLM invocation to produce a natural-language
/// explanation of a symbol's call flow.
pub struct ExplainUseCase {
    impact: ImpactAnalysisUseCase,
    call_graph: Arc<CallGraphUseCase>,
    snippet_lookup: SnippetLookupUseCase,
}

impl ExplainUseCase {
    pub fn new(
        impact: ImpactAnalysisUseCase,
        call_graph: Arc<CallGraphUseCase>,
        snippet_lookup: SnippetLookupUseCase,
    ) -> Self {
        Self { impact, call_graph, snippet_lookup }
    }

    /// Run the full explain pipeline and return the result.
    ///
    /// `chat_client` is provided by the caller so the choice of LLM backend
    /// (Anthropic, OpenAI, …) remains a connector-layer concern.
    pub async fn execute(
        &self,
        symbol: &str,
        repository: Option<&str>,
        chat_client: &dyn ChatClient,
    ) -> Result<ExplainResult, DomainError> {
        let analysis = self.impact.analyze(symbol, repository).await?;

        if analysis.total_affected == 0 {
            return Ok(ExplainResult {
                root_symbol: symbol.to_string(),
                explanation: format!(
                    "No callers found for '{}'. \
                     The symbol may be a root entry point or has not been indexed yet.",
                    symbol
                ),
                total_affected: 0,
                max_depth_reached: 0,
                symbol_sources: Vec::new(),
            });
        }

        // Locate the root symbol's definition via its callees (what it calls).
        let cg_query = match repository {
            Some(repo_id) => CallGraphQuery::new().with_repository(repo_id),
            None => CallGraphQuery::new(),
        };

        let root_source = {
            let callees = match self.call_graph.find_callees(&analysis.root_symbol, &cg_query).await {
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
                    let src = self
                        .snippet_lookup
                        .get_snippet(ref_.repository_id(), ref_.caller_file_path(), ref_.reference_line())
                        .await
                        .ok()
                        .flatten()
                        .map(|chunk| chunk.content().to_string());
                    src.map(|s| (ref_.caller_file_path().to_string(), s))
                }
                None => None,
            }
        };

        let (prompt, symbol_sources) =
            build_prompt(&analysis.root_symbol, root_source, &analysis.by_depth, &self.snippet_lookup).await;

        let explanation = chat_client
            .complete(SYSTEM_PROMPT, &prompt)
            .await
            .map_err(|e| DomainError::internal(format!("LLM call failed during explain: {e}")))?;

        Ok(ExplainResult {
            root_symbol: analysis.root_symbol,
            explanation,
            total_affected: analysis.total_affected,
            max_depth_reached: analysis.max_depth_reached,
            symbol_sources,
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

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

    // Lookup: (depth, symbol) → all matching nodes across all repos.
    let mut node_by_depth_symbol: HashMap<(usize, &str), Vec<&ImpactNode>> = HashMap::new();
    for node in &all_nodes {
        node_by_depth_symbol
            .entry((node.depth, node.symbol.as_str()))
            .or_default()
            .push(node);
    }

    let mut paths = Vec::new();
    for leaf in leaf_nodes {
        // Use an explicit stack so we can branch when multiple parents match.
        // Each entry is a partial path accumulated so far plus the node to extend from.
        let mut stack: Vec<(Vec<&ImpactNode>, &ImpactNode)> = vec![(vec![leaf], leaf)];

        while let Some((path, current)) = stack.pop() {
            match current.via_symbol.as_deref() {
                None => {
                    paths.push(path);
                }
                Some(via) => {
                    let parent_depth = current.depth.saturating_sub(1);
                    if let Some(candidates) = node_by_depth_symbol.get(&(parent_depth, via)) {
                        // Branch for every candidate so each matching symbol
                        // (across repos/classes) produces its own path.
                        let mut branched = false;
                        for &parent in candidates {
                            // Cycle guard: skip if this node is already in the path.
                            if !path.iter().any(|n| std::ptr::eq(*n, parent)) {
                                let mut new_path = path.clone();
                                new_path.push(parent);
                                stack.push((new_path, parent));
                                branched = true;
                            }
                        }
                        if !branched {
                            paths.push(path);
                        }
                    } else {
                        // No parent found — save the truncated path as-is.
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths
}

/// Construct the structured user prompt from the impact graph.
///
/// Returns the prompt string and the list of unique symbol sources that were
/// included — each entry is `(symbol, file_path, line, source)`.
async fn build_prompt(
    root_symbol: &str,
    root_source: Option<(String, String)>,
    by_depth: &[Vec<ImpactNode>],
    snippet_lookup: &SnippetLookupUseCase,
) -> (String, Vec<(String, String, u32, Option<String>)>) {
    let mut prompt = format!("# Call-flow explanation request: `{root_symbol}`\n\n");

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
        return (prompt, Vec::new());
    }

    let paths = reconstruct_paths(by_depth);
    let total_paths = paths.len();

    // Collect unique nodes to fetch source for (dedup by (symbol, file_path)).
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut nodes_to_fetch: Vec<&ImpactNode> = Vec::new();
    for path in &paths {
        for node in path {
            if seen.insert((node.symbol.as_str(), node.file_path.as_str())) {
                nodes_to_fetch.push(node);
            }
        }
    }

    // Fetch sources from the indexed store sequentially (cannot await inside a closure).
    let mut source_cache: HashMap<(&str, &str), Option<String>> = HashMap::new();
    for node in nodes_to_fetch {
        let src = snippet_lookup
            .get_snippet(&node.repository_id, &node.file_path, node.line)
            .await
            .ok()
            .flatten()
            .map(|chunk| chunk.content().to_string());
        source_cache.insert((node.symbol.as_str(), node.file_path.as_str()), src);
    }

    prompt.push_str(&format!("## Call paths ({total_paths} total)\n\n"));

    for (i, path) in paths.iter().enumerate() {
        let chain: String = path
            .iter()
            .map(|n| n.symbol.as_str())
            .chain(std::iter::once(root_symbol))
            .collect::<Vec<_>>()
            .join(" → ");
        prompt.push_str(&format!("### Path {} — `{}`\n\n", i + 1, chain));

        for node in path {
            let src_block =
                match source_cache.get(&(node.symbol.as_str(), node.file_path.as_str())) {
                    Some(Some(src)) => format!("```\n{src}\n```"),
                    _ => "_(source not available)_".to_string(),
                };
            prompt.push_str(&format!(
                "#### `{}` — `{}:{}`\n{}\n\n",
                node.symbol, node.file_path, node.line, src_block
            ));
        }
    }

    // Collect symbol sources in stable insertion order.
    let mut symbol_sources: Vec<(String, String, u32, Option<String>)> = Vec::new();
    let mut seen2: HashSet<(&str, &str)> = HashSet::new();
    for path in &paths {
        for node in path {
            if seen2.insert((node.symbol.as_str(), node.file_path.as_str())) {
                let src = source_cache
                    .get(&(node.symbol.as_str(), node.file_path.as_str()))
                    .cloned()
                    .flatten();
                symbol_sources.push((
                    node.symbol.clone(),
                    node.file_path.clone(),
                    node.line,
                    src,
                ));
            }
        }
    }

    (prompt, symbol_sources)
}
