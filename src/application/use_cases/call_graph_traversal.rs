//! Shared call-graph traversal primitives.
//!
//! `impact` (blast radius) and `context` (callers + callees) ask the same
//! question of the same edges: resolve a name to fully-qualified roots, then
//! walk outward one hop at a time. This module owns that walk once — the node
//! type, the root resolution, both BFS directions, and the path
//! reconstruction the renderers need — so the use cases layered on top stay
//! declarative and cannot drift apart.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::pattern_utils::build_fuzzy_pattern;
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

/// Stand-in symbol for a reference whose caller is module-level code with no
/// enclosing named symbol (e.g. `app.start()` at the top of an entry file).
pub const ANONYMOUS_SYMBOL: &str = "<anonymous>";

/// A single node in a call-graph BFS: one symbol reached from the query root,
/// together with the reference site that reached it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphNode {
    /// The symbol name, or [`ANONYMOUS_SYMBOL`] for an unattributed caller.
    pub symbol: String,
    /// Hop distance from the root symbol (1 = direct caller/callee, 2 = one hop further, …).
    pub depth: usize,
    /// File where the reference occurs.
    pub file_path: String,
    /// Line number of the reference in `file_path`.
    pub line: u32,
    /// Kind of reference relationship (e.g. "call", "type_reference").
    pub reference_kind: String,
    /// Repository that contains the reference.
    pub repository_id: String,
    /// Local alias at the import/require site, if the symbol was renamed.
    /// For example `bar` in `import { foo as bar }` or `const { foo: bar } = require(...)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_alias: Option<String>,
    /// The immediate parent in the BFS traversal — the symbol that led here.
    /// Always `Some` for nodes produced by [`bfs`]; the root itself is never
    /// emitted as a node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_symbol: Option<String>,
}

/// Which way to walk the call graph from a root symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Upward: who calls this symbol.
    Callers,
    /// Downward: what this symbol calls.
    Callees,
}

/// Breadth-first walk from `roots` in `direction`, grouped by hop depth.
///
/// The returned `by_depth[i]` holds the nodes at depth `i + 1`; the roots
/// themselves are never emitted. A global visited set deduplicates symbols and
/// guarantees termination on cyclic graphs, so every symbol appears exactly
/// once, at the shallowest depth it was reached.
pub async fn bfs(
    call_graph: &CallGraphUseCase,
    roots: &[String],
    query: &CallGraphQuery,
    direction: Direction,
) -> Result<Vec<Vec<CallGraphNode>>, DomainError> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    for symbol in roots {
        if visited.insert(symbol.clone()) {
            queue.push_back((symbol.clone(), 0));
        }
    }

    let mut by_depth: Vec<Vec<CallGraphNode>> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        let references = match direction {
            Direction::Callers => call_graph.find_callers(&current, query).await?,
            Direction::Callees => call_graph.find_callees(&current, query).await?,
        };
        if references.is_empty() {
            continue;
        }

        let next_depth = depth + 1;
        while by_depth.len() < next_depth {
            by_depth.push(Vec::new());
        }

        for reference in &references {
            let next_symbol = match direction {
                Direction::Callers => reference.caller_symbol(),
                Direction::Callees => Some(reference.callee_symbol()),
            };

            // An anonymous caller (top-level code with no enclosing function)
            // is reported so the user can see the call site, but not enqueued:
            // there is no named symbol to look up for the next hop. It is
            // deduplicated per file rather than per symbol, since every such
            // node shares the same placeholder name.
            let (symbol, visit_key, traversable) = match next_symbol {
                Some(name) => (name.to_string(), name.to_string(), true),
                None => (
                    ANONYMOUS_SYMBOL.to_string(),
                    format!(
                        "anon:{}:{}",
                        reference.repository_id(),
                        reference.caller_file_path()
                    ),
                    false,
                ),
            };

            if !visited.insert(visit_key) {
                continue;
            }

            by_depth[next_depth - 1].push(CallGraphNode {
                symbol: symbol.clone(),
                depth: next_depth,
                file_path: reference.reference_file_path().to_string(),
                line: reference.reference_line(),
                reference_kind: reference.reference_kind().to_string(),
                repository_id: reference.repository_id().to_string(),
                import_alias: reference.import_alias().map(str::to_string),
                via_symbol: Some(current.clone()),
            });

            if traversable {
                queue.push_back((symbol, next_depth));
            }
        }
    }

    Ok(by_depth)
}

/// `(total nodes, deepest non-empty hop level)` for a [`bfs`] result.
pub fn depth_summary(by_depth: &[Vec<CallGraphNode>]) -> (usize, usize) {
    let total = by_depth.iter().map(|d| d.len()).sum();
    let max_depth = by_depth
        .iter()
        .rposition(|d| !d.is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    (total, max_depth)
}

/// Maximum number of fully-qualified symbols a short name may resolve to.
/// Caps the ambiguity fan-out so a walk is never seeded with an unbounded
/// root set; when a name exceeds it the result is reported as capped rather
/// than silently covering an arbitrary alphabetical slice.
pub const RESOLVE_SYMBOLS_LIMIT: u32 = 100;

/// Resolve `symbol` to the fully-qualified names that match it, plus whether
/// the match set hit [`RESOLVE_SYMBOLS_LIMIT`]. Empty when nothing matches.
///
/// `is_regex` – when `true`, `symbol` is used as-is as a POSIX regex. When
///              `false` (the default) it is first tried as an exact match; if
///              that finds nothing it is wrapped as `.*<symbol>.*` so that
///              `codesearch impact load` still finds every FQN containing
///              "load".
pub async fn resolve_matches(
    call_graph: &CallGraphUseCase,
    symbol: &str,
    query: &CallGraphQuery,
    is_regex: bool,
) -> Result<(Vec<String>, bool), DomainError> {
    let (resolved, truncated) = resolve_capped(call_graph, symbol, query).await?;
    if !resolved.is_empty() || is_regex {
        // Regex mode takes the pattern at face value: no auto-wrap retry.
        return Ok((resolved, truncated));
    }

    let auto_pattern = format!(".*{}.*", build_fuzzy_pattern(symbol));
    let auto_query = query.clone().with_regex();
    debug!(
        symbol,
        auto_pattern, "call graph: exact match empty, retrying as substring regex"
    );
    resolve_capped(call_graph, &auto_pattern, &auto_query).await
}

/// Resolve `symbol` to the names a BFS should start from, together with the
/// label to display for the query.
///
/// When nothing resolves, the literal input is returned as the sole root so
/// the caller still gets a (necessarily empty) result keyed by what was asked.
/// Callers that need to tell "no match" apart from that use
/// [`resolve_matches`] directly.
pub async fn resolve_roots(
    call_graph: &CallGraphUseCase,
    symbol: &str,
    query: &CallGraphQuery,
    is_regex: bool,
) -> Result<(Vec<String>, String), DomainError> {
    let (resolved, truncated) = resolve_matches(call_graph, symbol, query, is_regex).await?;

    if resolved.is_empty() {
        debug!(
            symbol,
            "call graph: no rows match pattern — symbol may not be indexed"
        );
        return Ok((vec![symbol.to_string()], symbol.to_string()));
    }

    debug!(
        symbol,
        found = resolved.len(),
        "call graph: resolved {} root symbols",
        resolved.len()
    );
    let label = display_label(symbol, &resolved, truncated);
    Ok((resolved, label))
}

/// Resolve `pattern`, reporting whether the cap was hit. One row beyond the
/// cap is requested so that hitting it is detectable rather than invisible.
async fn resolve_capped(
    call_graph: &CallGraphUseCase,
    pattern: &str,
    query: &CallGraphQuery,
) -> Result<(Vec<String>, bool), DomainError> {
    let mut resolved = call_graph
        .resolve_symbols(pattern, query, RESOLVE_SYMBOLS_LIMIT + 1)
        .await?;
    let truncated = resolved.len() as u32 > RESOLVE_SYMBOLS_LIMIT;
    if truncated {
        resolved.truncate(RESOLVE_SYMBOLS_LIMIT as usize);
        warn!(
            pattern,
            cap = RESOLVE_SYMBOLS_LIMIT,
            "call graph: symbol resolved to more than {RESOLVE_SYMBOLS_LIMIT} FQNs; the walk \
             covers only the first {RESOLVE_SYMBOLS_LIMIT} — narrow the symbol for a complete \
             result"
        );
    }
    Ok((resolved, truncated))
}

/// Human-readable label for the analysed symbol. A single root is shown
/// verbatim; multiple roots show the count, with a `capped` note when
/// resolution hit the limit so the user knows the result is partial.
fn display_label(symbol: &str, resolved: &[String], truncated: bool) -> String {
    match resolved.len() {
        1 => resolved[0].clone(),
        n if truncated => format!("{symbol} ({n}+ symbols, capped at {RESOLVE_SYMBOLS_LIMIT})"),
        n => format!("{symbol} ({n} symbols)"),
    }
}

/// The outermost nodes of a BFS result: those that no other node was reached
/// through (i.e. that appear as nobody's `via_symbol`).
///
/// In a callers walk these are the entry points of each call chain. A symbol
/// can be reached from several unrelated places, so there may be many.
pub fn leaf_nodes<'a>(nodes: &[&'a CallGraphNode]) -> Vec<&'a CallGraphNode> {
    let via_set: HashSet<&str> = nodes
        .iter()
        .filter_map(|n| n.via_symbol.as_deref())
        .collect();
    nodes
        .iter()
        .copied()
        .filter(|n| !via_set.contains(n.symbol.as_str()))
        .collect()
}

/// Walk `leaf` back toward the query root through `via_symbol`, returning the
/// chain **leaf-first** (outermost node at index 0, closest-to-root last).
///
/// [`bfs`] emits each named symbol once, so a `(depth, symbol)` key identifies
/// exactly one node and the walk has a single deterministic parent at every
/// step. The one exception is [`ANONYMOUS_SYMBOL`], which is deduplicated per
/// file and so can repeat at a depth; the first node seen represents it. The
/// trade-off is that alternate routes to the same node are not enumerated —
/// this is one representative chain per leaf, not every possible path.
///
/// Use [`all_paths`] to trace every leaf: it indexes the node set once instead
/// of once per chain.
pub fn trace_path<'a>(
    leaf: &'a CallGraphNode,
    nodes: &[&'a CallGraphNode],
) -> Vec<&'a CallGraphNode> {
    walk_to_root(leaf, &path_index(nodes))
}

/// One representative chain per leaf (see [`trace_path`]), each leaf-first.
pub fn all_paths<'a>(nodes: &[&'a CallGraphNode]) -> Vec<Vec<&'a CallGraphNode>> {
    let index = path_index(nodes);
    leaf_nodes(nodes)
        .into_iter()
        .map(|leaf| walk_to_root(leaf, &index))
        .collect()
}

/// Lookup from `(depth, symbol)` to the node representing it.
type PathIndex<'a> = HashMap<(usize, &'a str), &'a CallGraphNode>;

fn path_index<'a>(nodes: &[&'a CallGraphNode]) -> PathIndex<'a> {
    let mut index = PathIndex::new();
    for &node in nodes {
        index
            .entry((node.depth, node.symbol.as_str()))
            .or_insert(node);
    }
    index
}

fn walk_to_root<'a>(leaf: &'a CallGraphNode, index: &PathIndex<'a>) -> Vec<&'a CallGraphNode> {
    let mut path = vec![leaf];
    let mut current = leaf;
    while let Some(via) = current.via_symbol.as_deref() {
        let parent_depth = current.depth.saturating_sub(1);
        if parent_depth == 0 {
            break; // the parent is the query root, which is not a node
        }
        match index.get(&(parent_depth, via)) {
            Some(&parent) => {
                path.push(parent);
                current = parent;
            }
            None => break,
        }
    }
    path
}
