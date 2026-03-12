use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::application::ChatClient;
use crate::domain::DomainError;

use super::snippet_lookup::SnippetLookupUseCase;
use super::symbol_context::{ContextNode, SymbolContext, SymbolContextUseCase};

const SYSTEM_PROMPT: &str = "\
You are a senior software engineer performing call-flow analysis. \
You will receive a root symbol together with its full bidirectional call context: \
the callers (who calls the root symbol and from where) and the callees \
(what the root symbol itself calls). \
Each caller path lists every symbol in the chain from the outermost entry point \
down to the root symbol, with source code for each hop.

You MUST analyse every symbol and every hop present in the provided paths \
as well as every callee listed. \
Do not summarise or skip any symbol. \
If multiple paths share a symbol, mention it once but note which paths it appears in.

Respond using exactly the four XML sections below. \
Do not add, rename, reorder, or skip any section. \
Do not output anything outside these XML tags. \
Replace the example content with your actual analysis.

<purpose>
[One paragraph. State what the root symbol does and why it exists, \
grounded in what both the full caller chain and the callees reveal about its role.

Example: compute_checksum validates the integrity of an incoming payload by \
hashing its bytes with SHA-256. The call chain shows it is invoked only after \
the payload has been decoded, making it the last defence against data corruption \
before the record is persisted.]
</purpose>

<data_and_control_flow>
[One entry per hop across ALL caller paths (outermost caller first, root symbol last), \
followed by one entry per direct callee of the root symbol. \
Every symbol in the provided paths and every callee must appear here. \
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
    payload bytes and compares the digest to the expected value.
• `compute_checksum` → `sha256_digest`  ← callee of root
  - compute_checksum calls sha256_digest to produce the hash bytes.]
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

    /// Run the full explain pipeline with streaming LLM output.
    ///
    /// Identical to [`Self::execute`] except that LLM tokens are forwarded to
    /// `token_tx` as they arrive.  The returned [`ExplainResult`] contains the
    /// complete (post-processed) explanation once the stream is exhausted.
    pub async fn execute_streaming(
        &self,
        symbol: &str,
        repository: Option<&str>,
        chat_client: &dyn ChatClient,
        is_regex: bool,
        token_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<ExplainResult, DomainError> {
        let ctx = self
            .context
            .get_context(symbol, repository, is_regex)
            .await?;

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

        let raw_explanation = chat_client
            .complete_stream(SYSTEM_PROMPT, &prompt, token_tx)
            .await
            .map_err(|e| {
                DomainError::internal(format!("LLM stream call failed during explain: {e}"))
            })?;

        Ok(ExplainResult {
            root_symbol: ctx.symbol,
            explanation: xml_to_markdown(&raw_explanation),
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
        ("purpose", "## Purpose"),
        ("data_and_control_flow", "## Data and control flow"),
        ("business_feature", "## Business feature"),
        (
            "key_patterns_and_dependencies",
            "## Key patterns and dependencies",
        ),
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
fn reconstruct_caller_paths(callers_by_depth: &[Vec<ContextNode>]) -> Vec<Vec<&ContextNode>> {
    let all_nodes: Vec<&ContextNode> = callers_by_depth.iter().flatten().collect();

    // children_map[sym] = nodes that list sym as their via_symbol.
    let mut children_map: HashMap<&str, Vec<&ContextNode>> = HashMap::new();
    for node in &all_nodes {
        if let Some(via) = node.via_symbol.as_deref() {
            children_map.entry(via).or_default().push(node);
        }
    }

    // Leaf nodes: outermost callers — not called by any other node in the set.
    let leaf_nodes: Vec<&ContextNode> = all_nodes
        .iter()
        .copied()
        .filter(|n| !children_map.contains_key(n.symbol.as_str()))
        .collect();

    let mut node_by_depth_symbol: HashMap<(usize, &str), Vec<&ContextNode>> = HashMap::new();
    for node in &all_nodes {
        node_by_depth_symbol
            .entry((node.depth, node.symbol.as_str()))
            .or_default()
            .push(node);
    }

    let mut paths = Vec::new();
    for leaf in leaf_nodes {
        let mut stack: Vec<(Vec<&ContextNode>, &ContextNode)> = vec![(vec![leaf], leaf)];

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
fn all_callees(callees_by_depth: &[Vec<ContextNode>]) -> Vec<&ContextNode> {
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

        // Explicit checklist.
        prompt.push_str("## Symbols you MUST cover in your response\n\n");
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
        prompt.push_str("## Symbols you MUST cover in your response\n\n");
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
            symbol_sources.push((symbol.to_string(), repo.to_string(), file_path.to_string(), *line, src));
        }
    }

    (prompt, symbol_sources)
}
