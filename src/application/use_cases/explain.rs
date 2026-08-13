use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::application::ChatClient;
use crate::domain::DomainError;

use super::call_graph_traversal::{leaf_nodes, CallGraphNode};
use super::snippet_lookup::SnippetLookupUseCase;
use super::symbol_context::{SymbolContext, SymbolContextUseCase};

const SYSTEM_PROMPT: &str = "\
You are analysing a code symbol for an AI coding agent that is about to work on it. \
The agent can read any file itself — your job is orientation, not documentation. \
Be dense and specific. Omit anything the agent could infer from a ten-second read \
of the source.

You receive a root symbol with its caller paths (entry point down to the root) \
and its callees, with source for each.

Respond using exactly the three XML sections below, in order, with nothing \
outside them. Obey the length budgets — going over is a failure, and it is \
always better to drop a weak item than to pad.

<summary>
[Two or three sentences, at most 60 words. What the root symbol does, and the \
user-visible capability its call chain serves. No hedging, no restating the signature.

Example: compute_checksum hashes a decoded payload and compares the digest to the \
expected value. It is the last integrity gate in the record-ingestion endpoint, \
run after validation and immediately before the record is persisted.]
</summary>

<flow>
[At most 6 bullets, one line each, ordered entry point first. Cover only \
load-bearing hops: where a decision is made, state changes, data is validated \
or transformed, or an external system is touched. Collapse pass-through wrappers \
into the neighbouring hop. If several caller paths converge, describe the shape \
once and name the variants.

Format: `<caller>` → `<callee>` — what is decided or transformed here.

Example:
• `handle_request` → `decode_payload` — parses the raw body; rejects malformed input before anything else runs.
• `validate_record` → `compute_checksum` — the integrity gate; a mismatch aborts the write.
• `compute_checksum` → `sha256_digest` — produces the digest that is compared and then stored.]
</flow>

<focus>
[Two to four bullets. Where an agent changing this symbol should look, and why. \
Each bullet names a concrete symbol or `file:line` from the provided context and \
states the risk in one clause: an invariant that must hold, a caller that would \
break, shared state, an error path, or an external contract. No generic advice \
(\"add tests\", \"be careful\") — if you have nothing concrete, emit fewer bullets.

Example:
• `validate_record` assumes the payload is already decoded — changing the argument type breaks both caller paths.
• `sha256_digest` output is persisted; altering the algorithm invalidates every stored record.]
</focus>";

/// Heading for the symbol list appended to the user prompt.
///
/// Framed as reference material rather than a coverage mandate: an earlier
/// "symbols you MUST cover" checklist made the model walk every entry, which
/// was the main source of over-long explanations.
const SYMBOL_INVENTORY_HEADING: &str =
    "## Symbol inventory (reference — mention only what carries the flow)\n\n";

/// Output produced by [`ExplainUseCase::execute`].
pub struct ExplainResult {
    pub root_symbol: String,
    pub explanation: String,
    pub total_affected: usize,
    pub max_depth_reached: usize,
    /// Unique symbols whose source chunks were sent to the LLM.
    /// Each entry is `(symbol, repository, file_path, line, source)`.
    pub symbol_sources: Vec<(String, String, String, u32, Option<String>)>,
    /// When non-empty, the input symbol matched multiple FQNs and the user
    /// must pick one.  `explanation` is empty and no LLM call was made.
    pub ambiguous_candidates: Vec<String>,
    /// Whether the query was interpreted as a regular expression.
    /// Used by the controller to tailor the disambiguation hint.
    pub is_regex: bool,
}

/// Orchestrates context analysis, call-graph traversal, snippet retrieval,
/// prompt construction, and LLM invocation to produce a natural-language
/// explanation of a symbol's full call context (callers + callees).
pub struct ExplainUseCase {
    context: Arc<SymbolContextUseCase>,
    snippet_lookup: SnippetLookupUseCase,
}

impl ExplainUseCase {
    pub fn new(context: Arc<SymbolContextUseCase>, snippet_lookup: SnippetLookupUseCase) -> Self {
        Self {
            context,
            snippet_lookup,
        }
    }

    /// Run the full explain pipeline and return the result.
    ///
    /// `chat_client` is provided by the caller so the choice of LLM backend
    /// (Anthropic, OpenAI, …) remains a connector-layer concern.
    /// `is_regex` is forwarded to the underlying context use case.
    pub async fn execute(
        &self,
        symbol: &str,
        repository: Option<&str>,
        chat_client: &dyn ChatClient,
        is_regex: bool,
    ) -> Result<ExplainResult, DomainError> {
        let ctx = self
            .context
            .get_context(symbol, repository, is_regex)
            .await?;

        // When the input matches multiple FQNs, ask the user to pick one before
        // running the expensive LLM call.
        if ctx.root_symbols.len() > 1 {
            return Ok(ExplainResult {
                root_symbol: symbol.to_string(),
                explanation: String::new(),
                total_affected: 0,
                max_depth_reached: 0,
                symbol_sources: Vec::new(),
                ambiguous_candidates: ctx.root_symbols,
                is_regex,
            });
        }

        if ctx.total_callers == 0 && ctx.total_callees == 0 {
            return Ok(ExplainResult {
                root_symbol: symbol.to_string(),
                explanation: format!(
                    "No callers or callees found for '{}'. \
                     The symbol may be isolated or has not been indexed yet.",
                    symbol
                ),
                total_affected: 0,
                max_depth_reached: 0,
                symbol_sources: Vec::new(),
                ambiguous_candidates: Vec::new(),
                is_regex,
            });
        }

        let total_affected = ctx.total_callers + ctx.total_callees;
        let max_depth_reached = ctx.max_caller_depth.max(ctx.max_callee_depth);

        let (prompt, symbol_sources) = build_prompt(&ctx, &self.snippet_lookup).await;

        let explanation = chat_client
            .complete(SYSTEM_PROMPT, &prompt)
            .await
            .map_err(|e| DomainError::internal(format!("LLM call failed during explain: {e}")))?;

        Ok(ExplainResult {
            root_symbol: ctx.symbol,
            explanation: xml_to_markdown(&explanation),
            total_affected,
            max_depth_reached,
            symbol_sources,
            ambiguous_candidates: Vec::new(),
            is_regex,
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Convert an XML-tagged LLM response into Markdown sections.
fn xml_to_markdown(s: &str) -> String {
    const SECTIONS: &[(&str, &str)] = &[
        ("summary", "## Summary"),
        ("flow", "## Flow"),
        ("focus", "## Where to focus"),
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

/// Strip Markdown bold/italic markers, leaving code spans intact.
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

    if !s.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn strip_emphasis_in_line(line: &str) -> String {
    remove_paired(line, "**")
}

fn remove_paired(s: &str, delim: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(delim) {
        out.push_str(&rest[..start]);
        rest = &rest[start + delim.len()..];
        if let Some(end) = rest.find(delim) {
            out.push_str(&rest[..end]);
            rest = &rest[end + delim.len()..];
        } else {
            out.push_str(delim);
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Reconstruct all caller paths from the BFS callers result.
///
/// Each path is ordered outermost-caller-first down to the direct caller
/// of the root symbol. The root itself is not included in the path — it is
/// rendered separately as the "root symbol" header.
fn reconstruct_caller_paths(callers_by_depth: &[Vec<CallGraphNode>]) -> Vec<Vec<&CallGraphNode>> {
    let all_nodes: Vec<&CallGraphNode> = callers_by_depth.iter().flatten().collect();

    // Outermost callers — not called by any other node in the set.
    let leaves = leaf_nodes(&all_nodes);

    // Unlike the single representative chain the renderers trace, an
    // explanation enumerates every distinct route, so a `(depth, symbol)` key
    // keeps all its candidates and the walk below branches across them.
    let mut node_by_depth_symbol: HashMap<(usize, &str), Vec<&CallGraphNode>> = HashMap::new();
    for node in &all_nodes {
        node_by_depth_symbol
            .entry((node.depth, node.symbol.as_str()))
            .or_default()
            .push(node);
    }

    let mut paths = Vec::new();
    for leaf in leaves {
        let mut stack: Vec<(Vec<&CallGraphNode>, &CallGraphNode)> = vec![(vec![leaf], leaf)];

        while let Some((path, current)) = stack.pop() {
            match current.via_symbol.as_deref() {
                None => {
                    paths.push(path);
                }
                Some(via) => {
                    let parent_depth = current.depth.saturating_sub(1);
                    if let Some(candidates) = node_by_depth_symbol.get(&(parent_depth, via)) {
                        let mut branched = false;
                        for &parent in candidates {
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
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths
}

/// Collect all callees across every BFS depth.
fn all_callees(callees_by_depth: &[Vec<CallGraphNode>]) -> Vec<&CallGraphNode> {
    callees_by_depth.iter().flatten().collect()
}

/// Construct the structured user prompt from the full symbol context.
///
/// Returns the prompt string and the list of unique symbol sources included —
/// each entry is `(symbol, repository, file_path, line, source)`.
async fn build_prompt(
    ctx: &SymbolContext,
    snippet_lookup: &SnippetLookupUseCase,
) -> (String, Vec<(String, String, String, u32, Option<String>)>) {
    let root_symbol = &ctx.symbol;
    let mut prompt = format!("# Call-flow explanation request: `{root_symbol}`\n\n");

    // ── Root symbol source ────────────────────────────────────────────────────
    // Look it up via its first callee's caller_file_path if available,
    // otherwise via its first caller's reference.
    let root_source: Option<(String, String)> = {
        // Prefer the caller side: first caller node stores (file_path, line)
        // as the call-site inside the calling function — but the caller's
        // via_symbol points back to the root, so the root's file is
        // caller.file_path for depth-1 callers.
        // Actually the cleanest source: look at depth-1 callee nodes —
        // their file_path is the root's file (they are called from there).
        // But that's the root's file at the call-site, not the definition.
        //
        // Best approach: use the snippet_lookup with the root symbol name directly.
        let repo = ctx
            .callers_by_depth
            .first()
            .and_then(|d| d.first())
            .map(|n| n.repository_id.as_str())
            .unwrap_or("");
        snippet_lookup
            .get_snippet_for_symbol(repo, root_symbol)
            .await
            .ok()
            .flatten()
            .map(|chunk| (chunk.file_path().to_string(), chunk.content().to_string()))
    };

    match root_source {
        Some((file_path, ref src)) => {
            prompt.push_str(&format!(
                "## Root symbol — `{root_symbol}`\n\
                 Source from `{file_path}`:\n\
                 ```\n{src}\n```\n\n"
            ));
        }
        None => {
            prompt.push_str(&format!(
                "## Root symbol — `{root_symbol}`\n\
                 _(source not available)_\n\n"
            ));
        }
    }

    let caller_paths = reconstruct_caller_paths(&ctx.callers_by_depth);
    let callees = all_callees(&ctx.callees_by_depth);

    // ── Collect unique nodes to fetch source for ──────────────────────────────
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut nodes_to_fetch: Vec<(&str, &str, u32, &str, bool)> = Vec::new(); // (symbol, file, line, repo, is_callee)

    for path in &caller_paths {
        for node in path {
            if seen.insert((node.symbol.as_str(), node.file_path.as_str())) {
                nodes_to_fetch.push((
                    &node.symbol,
                    &node.file_path,
                    node.line,
                    &node.repository_id,
                    false,
                ));
            }
        }
    }
    for node in &callees {
        if seen.insert((node.symbol.as_str(), node.file_path.as_str())) {
            nodes_to_fetch.push((
                &node.symbol,
                &node.file_path,
                node.line,
                &node.repository_id,
                true,
            ));
        }
    }

    // ── Fetch sources ─────────────────────────────────────────────────────────
    // key: (symbol, file_path)
    let mut source_cache: HashMap<(String, String), Option<String>> = HashMap::new();
    for (symbol, file_path, line, repo, is_callee) in &nodes_to_fetch {
        let key = (symbol.to_string(), file_path.to_string());
        let result = if *is_callee {
            snippet_lookup
                .get_snippet_for_symbol(repo, symbol)
                .await
                .ok()
                .flatten()
                .map(|c| c.content().to_string())
        } else {
            snippet_lookup
                .get_snippet(repo, file_path, *line)
                .await
                .ok()
                .flatten()
                .map(|c| c.content().to_string())
        };
        source_cache.insert(key, result);
    }

    // ── Caller paths section ──────────────────────────────────────────────────
    if !caller_paths.is_empty() {
        let total_paths = caller_paths.len();
        prompt.push_str(&format!("## Caller paths ({total_paths} total)\n\n"));

        let mut seen_symbols: HashSet<String> = HashSet::from([root_symbol.clone()]);
        let mut all_symbols: Vec<String> = vec![root_symbol.clone()];

        for (i, path) in caller_paths.iter().enumerate() {
            let chain: String = path
                .iter()
                .map(|n| n.symbol.as_str())
                .chain(std::iter::once(root_symbol.as_str()))
                .collect::<Vec<_>>()
                .join(" → ");
            prompt.push_str(&format!("### Path {} — `{}`\n\n", i + 1, chain));

            for node in path {
                let key = (node.symbol.clone(), node.file_path.clone());
                let src_block = match source_cache.get(&key) {
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

        // ── Callees section ───────────────────────────────────────────────────
        if !callees.is_empty() {
            prompt.push_str(&format!(
                "## Callees of `{root_symbol}` ({} total)\n\n",
                callees.len()
            ));
            for node in &callees {
                let key = (node.symbol.clone(), node.file_path.clone());
                let src_block = match source_cache.get(&key) {
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

        // Reference inventory — deliberately NOT a coverage mandate: the
        // response is meant to orient, not to enumerate every symbol.
        prompt.push_str(SYMBOL_INVENTORY_HEADING);
        for sym in &all_symbols {
            prompt.push_str(&format!("- `{sym}`\n"));
        }
        prompt.push('\n');
    } else if !callees.is_empty() {
        // No callers, only callees.
        prompt.push_str(&format!(
            "## Callees of `{root_symbol}` ({} total)\n\n",
            callees.len()
        ));
        let mut all_symbols: Vec<String> = vec![root_symbol.clone()];
        for node in &callees {
            let key = (node.symbol.clone(), node.file_path.clone());
            let src_block = match source_cache.get(&key) {
                Some(Some(src)) => format!("```\n{src}\n```"),
                _ => "_(source not available)_".to_string(),
            };
            prompt.push_str(&format!(
                "#### `{}` — `{}:{}`\n{}\n\n",
                node.symbol, node.file_path, node.line, src_block
            ));
            all_symbols.push(node.symbol.clone());
        }
        prompt.push_str(SYMBOL_INVENTORY_HEADING);
        for sym in &all_symbols {
            prompt.push_str(&format!("- `{sym}`\n"));
        }
        prompt.push('\n');
    }

    // ── symbol_sources for the ExplainResult ─────────────────────────────────
    let mut symbol_sources: Vec<(String, String, String, u32, Option<String>)> = Vec::new();
    let mut seen3: HashSet<(String, String)> = HashSet::new();
    for (symbol, file_path, line, repo, _is_callee) in &nodes_to_fetch {
        let key = (symbol.to_string(), file_path.to_string());
        if seen3.insert(key.clone()) {
            let src = source_cache.get(&key).cloned().flatten();
            symbol_sources.push((
                symbol.to_string(),
                repo.to_string(),
                file_path.to_string(),
                *line,
                src,
            ));
        }
    }

    (prompt, symbol_sources)
}
