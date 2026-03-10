use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use tracing::debug;

use crate::application::use_cases::pattern_utils::build_fuzzy_pattern;
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

pub const ANONYMOUS_SYMBOL: &str = "<anonymous>";

/// Maximum number of fully-qualified symbols to resolve from a short name during
/// fallback resolution. Caps the ambiguity fan-out without blocking common cases.
const RESOLVE_SYMBOLS_LIMIT: u32 = 10;

/// A single node in the impact (blast-radius) graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactNode {
    /// The affected symbol name.
    pub symbol: String,
    /// Hop distance from the root symbol (1 = direct caller, 2 = caller of caller, …).
    pub depth: usize,
    /// File where the reference occurs.
    pub file_path: String,
    /// Line number where the reference occurs in `file_path`.
    pub line: u32,
    /// Kind of reference relationship (e.g. "call", "type_reference").
    pub reference_kind: String,
    /// Repository that contains the caller symbol.
    pub repository_id: String,
    /// Local alias at the import/require site, if the root symbol was renamed.
    /// For example `bar` in `import { foo as bar }` or `const { foo: bar } = require(...)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_alias: Option<String>,
    /// The immediate parent symbol in the BFS traversal (i.e. the symbol that led to this one).
    /// `None` only for the root symbol itself; always `Some` for every other node.
    pub via_symbol: Option<String>,
}

/// Full blast-radius report for a symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactAnalysis {
    /// Display label for the analysed symbol (may contain a summary like
    /// `"foo (3 symbols)"` when multiple FQNs were resolved). Use this
    /// field for UI rendering only.
    pub root_symbol: String,
    /// The fully-qualified symbol names that were used as BFS roots.
    /// Use this field for programmatic lookups (e.g. further DB queries).
    pub root_symbols: Vec<String>,
    /// Total number of transitively affected symbols (excluding the root).
    pub total_affected: usize,
    /// Deepest hop level reached that contained at least one result.
    pub max_depth_reached: usize,
    /// Affected symbols grouped by hop depth (index 0 = depth 1 = direct callers).
    pub by_depth: Vec<Vec<ImpactNode>>,
}

impl ImpactAnalysis {
    /// Leaf nodes: symbols that are not the `via_symbol` of any other node.
    ///
    /// These are the furthest BFS-hop callers — the "entry-point" roots of each
    /// call chain.  A symbol can be called from multiple unrelated spots, so
    /// there may be several leaves.
    pub fn leaf_nodes(&self) -> Vec<&ImpactNode> {
        use std::collections::HashSet;
        let all: Vec<&ImpactNode> = self.by_depth.iter().flatten().collect();
        let via_set: HashSet<&str> = all.iter().filter_map(|n| n.via_symbol.as_deref()).collect();
        all.into_iter()
            .filter(|n| !via_set.contains(n.symbol.as_str()))
            .collect()
    }

    /// Build the call chain for `leaf` by walking `via_symbol` back toward the
    /// root.  Returns nodes in **leaf-first** order (entry point at index 0,
    /// closest-to-root at the end).
    pub fn path_for_leaf<'a>(&'a self, leaf: &'a ImpactNode) -> Vec<&'a ImpactNode> {
        use std::collections::HashMap;
        let mut by_depth_sym: HashMap<(usize, &str), &ImpactNode> = HashMap::new();
        for node in self.by_depth.iter().flatten() {
            by_depth_sym
                .entry((node.depth, node.symbol.as_str()))
                .or_insert(node);
        }

        let mut path: Vec<&'a ImpactNode> = vec![leaf];
        let mut current = leaf;
        loop {
            let via = match current.via_symbol.as_deref() {
                Some(v) => v,
                None => break,
            };
            let parent_depth = current.depth.saturating_sub(1);
            if parent_depth == 0 {
                break;
            }
            match by_depth_sym.get(&(parent_depth, via)) {
                Some(&parent) => {
                    path.push(parent);
                    current = parent;
                }
                None => break,
            }
        }
        path // leaf-first
    }
}

/// Use case: BFS outward from a symbol through the call graph to identify
/// every symbol that would be affected if the root symbol changes.
pub struct ImpactAnalysisUseCase {
    call_graph: Arc<CallGraphUseCase>,
}

impl ImpactAnalysisUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self { call_graph }
    }

    /// Compute blast radius.
    ///
    /// `symbol`        – symbol name or substring to analyse (e.g. `"authenticate"`),
    ///                   or a full POSIX regex when `is_regex` is `true`.
    /// `repository_id` – optional repository filter.
    /// `is_regex`      – when `true`, `symbol` is used as-is as a regex pattern
    ///                   (no auto-wrapping).  When `false` (the default), the
    ///                   symbol is first tried as an exact match; if that returns
    ///                   nothing it is automatically wrapped as `.*<symbol>.*` so
    ///                   that `codesearch impact load` finds every FQN containing
    ///                   the substring "load".  Pass `--regex` to supply your own
    ///                   full pattern without auto-wrapping.
    ///
    /// When multiple symbols resolve, results from **all** of them are merged.
    pub async fn analyze(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
        is_regex: bool,
    ) -> Result<ImpactAnalysis, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo_id) = repository_id {
            query = query.with_repository(repo_id);
        }
        if is_regex {
            query = query.with_regex();
        }

        // Determine the set of root symbols to BFS from and a display label.
        let (root_symbols, display_symbol): (Vec<String>, String) = if is_regex {
            // Regex mode: always resolve via pattern matching; never try an exact hit.
            let resolved = self
                .call_graph
                .resolve_symbols(symbol, &query, RESOLVE_SYMBOLS_LIMIT)
                .await?;
            if resolved.is_empty() {
                (vec![symbol.to_string()], symbol.to_string())
            } else if resolved.len() == 1 {
                let s = resolved[0].clone();
                (resolved, s)
            } else {
                let display = format!("{} ({} symbols)", symbol, resolved.len());
                (resolved, display)
            }
        } else {
            // Auto-wrap mode: first check if the symbol exists in the call graph
            // using resolve_symbols (which queries both callee_symbol and
            // caller_symbol).  This correctly handles root entry-point symbols
            // that have zero callers but appear as caller_symbol — find_callers
            // would return empty for them, incorrectly triggering fuzzy expansion.
            let exact_resolved = self
                .call_graph
                .resolve_symbols(symbol, &query, RESOLVE_SYMBOLS_LIMIT)
                .await?;
            if !exact_resolved.is_empty() {
                debug!(
                    symbol,
                    found = exact_resolved.len(),
                    "impact: exact-match found {} symbols",
                    exact_resolved.len()
                );
                let display = if exact_resolved.len() == 1 {
                    exact_resolved[0].clone()
                } else {
                    format!("{} ({} symbols)", symbol, exact_resolved.len())
                };
                (exact_resolved, display)
            } else {
                let auto_pattern = format!(".*{}.*", build_fuzzy_pattern(symbol));
                let auto_query = query.clone().with_regex();
                debug!(
                    symbol,
                    auto_pattern, "impact: exact-match empty, trying auto-wrap regex"
                );
                let resolved = self
                    .call_graph
                    .resolve_symbols(&auto_pattern, &auto_query, RESOLVE_SYMBOLS_LIMIT)
                    .await?;
                debug!(
                    symbol,
                    resolved_count = resolved.len(),
                    ?resolved,
                    "impact: auto-wrap resolved"
                );
                if resolved.is_empty() {
                    debug!(
                        symbol,
                        "impact: no rows match pattern — symbol may not be indexed"
                    );
                    (vec![symbol.to_string()], symbol.to_string())
                } else if resolved.len() == 1 {
                    let s = resolved[0].clone();
                    (resolved, s)
                } else {
                    let display = format!("{} ({} symbols)", symbol, resolved.len());
                    (resolved, display)
                }
            }
        };

        let mut visited: HashSet<String> = HashSet::new();

        // Seed the BFS with every root symbol.
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        for sym in &root_symbols {
            if visited.insert(sym.clone()) {
                queue.push_back((sym.clone(), 0));
            }
        }

        // by_depth[i] holds nodes at depth i+1
        let mut by_depth: Vec<Vec<ImpactNode>> = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            let callers = self.call_graph.find_callers(&current, &query).await?;
            if callers.is_empty() {
                continue;
            }

            let next_depth = depth + 1;

            // Ensure the depth level exists.
            while by_depth.len() < next_depth {
                by_depth.push(Vec::new());
            }

            for reference in &callers {
                match reference.caller_symbol() {
                    None => {
                        // Anonymous caller (top-level / module-level code with no enclosing
                        // function).  Include it in the impact report so the user can see it,
                        // but don't enqueue it for further traversal – there is no named
                        // symbol to look up.
                        let anon_key = format!(
                            "anon:{}:{}",
                            reference.repository_id(),
                            reference.caller_file_path()
                        );
                        if visited.contains(&anon_key) {
                            continue;
                        }
                        visited.insert(anon_key);
                        by_depth[next_depth - 1].push(ImpactNode {
                            symbol: ANONYMOUS_SYMBOL.to_string(),
                            depth: next_depth,
                            file_path: reference.reference_file_path().to_string(),
                            line: reference.reference_line(),
                            reference_kind: reference.reference_kind().to_string(),
                            repository_id: reference.repository_id().to_string(),
                            import_alias: reference.import_alias().map(str::to_string),
                            via_symbol: Some(current.clone()),
                        });
                    }
                    Some(caller_sym) => {
                        let caller_sym = caller_sym.to_string();

                        if visited.contains(&caller_sym) {
                            continue;
                        }
                        visited.insert(caller_sym.clone());

                        by_depth[next_depth - 1].push(ImpactNode {
                            symbol: caller_sym.clone(),
                            depth: next_depth,
                            file_path: reference.reference_file_path().to_string(),
                            line: reference.reference_line(),
                            reference_kind: reference.reference_kind().to_string(),
                            repository_id: reference.repository_id().to_string(),
                            import_alias: reference.import_alias().map(str::to_string),
                            via_symbol: Some(current.clone()),
                        });

                        queue.push_back((caller_sym, next_depth));
                    }
                }
            }
        }

        let total_affected = by_depth.iter().map(|d| d.len()).sum();
        let max_depth_reached = by_depth
            .iter()
            .rposition(|d| !d.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);

        Ok(ImpactAnalysis {
            root_symbol: display_symbol,
            root_symbols,
            total_affected,
            max_depth_reached,
            by_depth,
        })
    }
}
