use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{debug, warn};

use crate::application::{CallGraphQuery, CallGraphUseCase, ChatClient};
use crate::domain::DomainError;

use super::impact_analysis::{ImpactAnalysisUseCase, ImpactNode};
use super::snippet_lookup::SnippetLookupUseCase;

const SYSTEM_PROMPT: &str = "\
You are a senior software engineer performing call-flow analysis. \
You will receive a root symbol and one or more call paths. \
Each path lists every caller in the chain from the outermost entry point down to the root symbol, \
with source code for each hop.

You MUST analyse every symbol and every hop present in the provided call paths. \
Do not summarise or skip any symbol. \
If multiple paths share a symbol, mention it once but note which paths it appears in.

Respond using exactly the four XML sections below. \
Do not add, rename, reorder, or skip any section. \
Do not output anything outside these XML tags. \
Replace the example content with your actual analysis.

<purpose>
[One paragraph. State what the root symbol does and why it exists, \
grounded in what the full call chain reveals about its role.

Example: compute_checksum validates the integrity of an incoming payload by \
hashing its bytes with SHA-256. The call chain shows it is invoked only after \
the payload has been decoded, making it the last defence against data corruption \
before the record is persisted.]
</purpose>

<data_and_control_flow>
[One entry per hop across ALL call paths, outermost caller first, root symbol last. \
Every symbol in the provided paths must appear in at least one entry. \
Use this format for each entry:

• `<caller>` → `<callee>`
  - What the caller validates, prepares, or checks before the call.
  - What arguments it passes and what the callee does with them.
  - Any conditional or secondary calls made within this hop.

Example:
• `handle_request` → `decode_payload`
  - Receives the raw HTTP body and the request context.
  - Calls `decode_payload(body)` to deserialise the bytes into a Record struct.
• `decode_payload` → `validate_record`
  - Passes the Record to `validate_record(record)`, which checks required fields \
    and rejects malformed input.
• `validate_record` → `compute_checksum`
  - Passes the validated Record to `compute_checksum(record)`, which hashes the \
    payload bytes and compares the digest to the expected value.]
</data_and_control_flow>

<business_feature>
[One paragraph describing the end-to-end user-visible capability the entire call chain implements. \
Mention the entry-point symbols (API endpoints, CLI commands, event handlers, etc.) \
and the root symbol's contribution to that capability.

Example: The chain implements the record-ingestion API endpoint exposed by handle_request. \
A client posts a JSON payload; the system decodes, validates, and checksums it before \
writing it to the database. compute_checksum is the integrity gate that ensures \
no corrupted record is ever persisted.]
</business_feature>

<key_patterns_and_dependencies>
[One entry per notable abstraction, external service, framework, or design pattern \
that appears in the provided source snippets. \
Use this format for each entry:

• `<item name>` (<source or type>) — used by `<symbol>`
  - What the item is.
  - Why it is used here.

Example:
• `SHA-256` (ring crate) — used by `compute_checksum`
  - Cryptographic hash function that produces a deterministic 256-bit digest.
  - Used to verify payload integrity before persisting the record.
• Repository pattern — used by `validate_record`
  - `validate_record` depends on a `RecordRepository` trait rather than a concrete type.
  - Decouples business validation logic from the storage backend.]
</key_patterns_and_dependencies>";

/// Output produced by [`ExplainUseCase::execute`].
pub struct ExplainResult {
    pub root_symbol: String,
    pub explanation: String,
    pub total_affected: usize,
    pub max_depth_reached: usize,
    /// Unique symbols whose source chunks were sent to the LLM.
    /// Each entry is `(symbol, file_path, line, source)`.
    pub symbol_sources: Vec<(String, String, u32, Option<String>)>,
    /// When non-empty, the input symbol matched multiple FQNs and the user
    /// must pick one.  `explanation` is empty and no LLM call was made.
    pub ambiguous_candidates: Vec<String>,
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
    /// `is_regex` is forwarded to the underlying impact analysis; see
    /// [`ImpactAnalysisUseCase::analyze`] for semantics.
    pub async fn execute(
        &self,
        symbol: &str,
        repository: Option<&str>,
        chat_client: &dyn ChatClient,
        is_regex: bool,
    ) -> Result<ExplainResult, DomainError> {
        let analysis = self.impact.analyze(symbol, repository, is_regex).await?;

        // When the input matches multiple FQNs, ask the user to pick one before
        // running the expensive LLM call.  There is no meaningful "explain all"
        // mode: the root-source lookup only covers one symbol anyway, and merging
        // callers from multiple unrelated FQNs produces a confusing result.
        if analysis.root_symbols.len() > 1 {
            return Ok(ExplainResult {
                root_symbol: symbol.to_string(),
                explanation: String::new(),
                total_affected: 0,
                max_depth_reached: 0,
                symbol_sources: Vec::new(),
                ambiguous_candidates: analysis.root_symbols,
            });
        }

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
                ambiguous_candidates: Vec::new(),
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
            explanation: xml_to_markdown(&explanation),
            total_affected: analysis.total_affected,
            max_depth_reached: analysis.max_depth_reached,
            symbol_sources,
            ambiguous_candidates: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Convert an XML-tagged LLM response into Markdown sections.
///
/// Extracts the four expected tags and renders them under `##` headings.
/// Falls back to returning the raw response if no recognised tags are found,
/// so older/non-conforming model output still passes through.
fn xml_to_markdown(s: &str) -> String {
    const SECTIONS: &[(&str, &str)] = &[
        ("purpose", "## Purpose"),
        ("data_and_control_flow", "## Data and control flow"),
        ("business_feature", "## Business feature"),
        ("key_patterns_and_dependencies", "## Key patterns and dependencies"),
    ];

    let mut out = String::new();
    for &(tag, heading) in SECTIONS {
        if let Some(content) = extract_xml_tag(s, tag) {
            out.push_str(heading);
            out.push('\n');
            out.push_str(strip_markdown_emphasis(content.trim()).trim_end());
            out.push_str("\n\n");
        }
    }

    if out.is_empty() {
        // No XML tags found — fall back to stripping emphasis on the raw response.
        strip_markdown_emphasis(s)
    } else {
        out.trim_end().to_string()
    }
}

/// Return the text content between `<tag>` and `</tag>`, if present.
fn extract_xml_tag<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(open.as_str())? + open.len();
    let end = s[start..].find(close.as_str()).map(|i| start + i)?;
    Some(&s[start..end])
}

/// Strip Markdown bold (`**text**`, `__text__`) and italic (`*text*`, `_text_`)
/// markers from `s`, leaving the inner text intact.
///
/// Operates line-by-line so that fenced code blocks (` ``` `) are left
/// untouched: lines inside a code fence are passed through verbatim.
fn strip_markdown_emphasis(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_code_fence = false;

    for line in s.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_code_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(&strip_emphasis_in_line(line));
        out.push('\n');
    }

    // Preserve the original trailing-newline behaviour.
    if !s.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Remove paired `**` delimiters from a single line, leaving `_`, `__`, and `*` intact
/// (underscores and single asterisks can be significant in code tokens).
fn strip_emphasis_in_line(line: &str) -> String {
    remove_paired(line, "**")
}

/// Remove all paired occurrences of `delim` from `s`, keeping the inner text.
fn remove_paired(s: &str, delim: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(delim) {
        out.push_str(&rest[..start]);
        rest = &rest[start + delim.len()..];
        // Look for the closing delimiter on the same line.
        if let Some(end) = rest.find(delim) {
            out.push_str(&rest[..end]);
            rest = &rest[end + delim.len()..];
        } else {
            // No closing delimiter — emit the opening one verbatim and stop.
            out.push_str(delim);
            break;
        }
    }
    out.push_str(rest);
    out
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
        match snippet_lookup
            .get_snippet(&node.repository_id, &node.file_path, node.line)
            .await
        {
            Ok(chunk) => {
                let src = chunk.map(|c| c.content().to_string());
                source_cache.insert((node.symbol.as_str(), node.file_path.as_str()), src);
            }
            Err(e) => {
                debug!(
                    error = %e,
                    repository_id = %node.repository_id,
                    file_path = %node.file_path,
                    line = %node.line,
                    symbol = %node.symbol,
                    "snippet lookup failed; source will be unavailable"
                );
                source_cache.insert((node.symbol.as_str(), node.file_path.as_str()), None);
            }
        }
    }

    prompt.push_str(&format!("## Call paths ({total_paths} total)\n\n"));

    // Collect the full symbol roster so the model knows exactly what to cover.
    let mut all_symbols: Vec<String> = vec![root_symbol.to_string()];
    let mut seen_symbols: HashSet<String> = HashSet::from([root_symbol.to_string()]);

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
            if seen_symbols.insert(node.symbol.clone()) {
                all_symbols.push(node.symbol.clone());
            }
        }
    }

    // Explicit checklist so the model cannot skip any symbol.
    prompt.push_str("## Symbols you MUST cover in your response\n\n");
    for sym in &all_symbols {
        prompt.push_str(&format!("- `{sym}`\n"));
    }
    prompt.push('\n');

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
