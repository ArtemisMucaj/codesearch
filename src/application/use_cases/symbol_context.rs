use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::use_cases::pattern_utils::build_fuzzy_pattern;
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

/// A single node in the context (caller or callee) BFS graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextNode {
    /// The symbol name.
    pub symbol: String,
    /// Hop distance from the root symbol (1 = direct caller/callee, 2 = one hop further, …).
    pub depth: usize,
    /// File where the reference occurs.
    pub file_path: String,
    /// Line number of the reference.
    pub line: u32,
    /// Kind of reference (e.g. "call", "type_reference").
    pub reference_kind: String,
    /// Repository that contains the symbol.
    pub repository_id: String,
    /// Local alias at the import/require site, if the symbol was renamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_alias: Option<String>,
    /// The immediate parent symbol in the BFS traversal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_symbol: Option<String>,
}

/// Depth-grouped BFS view of a symbol's call-graph relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolContext {
    /// Display label for the analysed symbol.
    pub symbol: String,
    /// The fully-qualified symbol names used as BFS roots.
    pub root_symbols: Vec<String>,
    /// Callers BFS: index 0 = depth 1 = direct callers.
    pub callers_by_depth: Vec<Vec<ContextNode>>,
    /// Total number of transitively calling symbols (excluding the root).
    pub total_callers: usize,
    /// Deepest hop level reached that contained at least one caller.
    pub max_caller_depth: usize,
    /// Callees BFS: index 0 = depth 1 = direct callees.
    pub callees_by_depth: Vec<Vec<ContextNode>>,
    /// Total number of transitively called symbols (excluding the root).
    pub total_callees: usize,
    /// Deepest hop level reached that contained at least one callee.
    pub max_callee_depth: usize,
}

/// Default maximum number of fully-qualified symbols to resolve from a short name
/// when the exact match returns no results. Caps the ambiguity fan-out.
const FALLBACK_RESOLUTION_LIMIT: u32 = 10;

/// Use case: return a complete depth-grouped caller + callee BFS view for a named symbol.
pub struct SymbolContextUseCase {
    call_graph: Arc<CallGraphUseCase>,
}

impl SymbolContextUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self { call_graph }
    }

    /// Fetch callers and callees for `symbol` via parallel BFS passes and combine them.
    ///
    /// `repository_id` – optional filter.
    /// `is_regex`      – when `true`, `symbol` is used as-is as a POSIX regex
    ///                   (no auto-wrapping). When `false` (the default), the
    ///                   symbol is first tried as an exact match; if that returns
    ///                   nothing it is automatically wrapped as `.*<symbol>.*`.
    pub async fn get_context(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
        is_regex: bool,
    ) -> Result<SymbolContext, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo_id) = repository_id {
            query = query.with_repository(repo_id);
        }
        if is_regex {
            query = query.with_regex();
        }

        // Resolve root symbols using the same exact → fuzzy fallback logic as ImpactAnalysis.
        let (root_symbols, display_symbol): (Vec<String>, String) = if is_regex {
            let resolved = self
                .call_graph
                .resolve_symbols(symbol, &query, FALLBACK_RESOLUTION_LIMIT)
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
            let exact_resolved = self
                .call_graph
                .resolve_symbols(symbol, &query, FALLBACK_RESOLUTION_LIMIT)
                .await?;
            if !exact_resolved.is_empty() {
                let display = if exact_resolved.len() == 1 {
                    exact_resolved[0].clone()
                } else {
                    format!("{} ({} symbols)", symbol, exact_resolved.len())
                };
                (exact_resolved, display)
            } else {
                let auto_pattern = format!(".*{}.*", build_fuzzy_pattern(symbol));
                let auto_query = query.clone().with_regex();
                let resolved = self
                    .call_graph
                    .resolve_symbols(&auto_pattern, &auto_query, FALLBACK_RESOLUTION_LIMIT)
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
            }
        };

        // Run both BFS passes in parallel.
        let (callers_result, callees_result) = tokio::join!(
            self.run_callers_bfs(&root_symbols, &query),
            self.run_callees_bfs(&root_symbols, &query),
        );
        let callers_by_depth = callers_result?;
        let callees_by_depth = callees_result?;

        let total_callers = callers_by_depth.iter().map(|d| d.len()).sum();
        let max_caller_depth = callers_by_depth
            .iter()
            .rposition(|d| !d.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);

        let total_callees = callees_by_depth.iter().map(|d| d.len()).sum();
        let max_callee_depth = callees_by_depth
            .iter()
            .rposition(|d| !d.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);

        Ok(SymbolContext {
            symbol: display_symbol,
            root_symbols,
            callers_by_depth,
            total_callers,
            max_caller_depth,
            callees_by_depth,
            total_callees,
            max_callee_depth,
        })
    }

    /// BFS upward through callers (mirrors ImpactAnalysisUseCase::analyze).
    ///
    /// Starting from every root symbol, repeatedly calls `find_callers` to walk
    /// up the call chain. Results are grouped by depth; visited-set deduplication
    /// prevents cycles and infinite loops.
    async fn run_callers_bfs(
        &self,
        root_symbols: &[String],
        query: &CallGraphQuery,
    ) -> Result<Vec<Vec<ContextNode>>, DomainError> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        for sym in root_symbols {
            if visited.insert(sym.clone()) {
                queue.push_back((sym.clone(), 0));
            }
        }

        // by_depth[i] holds nodes at depth i+1
        let mut by_depth: Vec<Vec<ContextNode>> = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            let callers = self.call_graph.find_callers(&current, query).await?;
            if callers.is_empty() {
                continue;
            }

            let next_depth = depth + 1;

            while by_depth.len() < next_depth {
                by_depth.push(Vec::new());
            }

            for reference in &callers {
                match reference.caller_symbol() {
                    None => {
                        // Anonymous caller — include but don't enqueue further.
                        let anon_key = format!(
                            "anon:{}:{}",
                            reference.repository_id(),
                            reference.caller_file_path()
                        );
                        if visited.contains(&anon_key) {
                            continue;
                        }
                        visited.insert(anon_key);
                        by_depth[next_depth - 1].push(ContextNode {
                            symbol: "<anonymous>".to_string(),
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
                        by_depth[next_depth - 1].push(ContextNode {
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

        Ok(by_depth)
    }

    /// BFS downward through callees (symmetric to `run_callers_bfs`).
    ///
    /// Starting from every root symbol, repeatedly calls `find_callees` to walk
    /// down the call chain. Results are grouped by depth; visited-set deduplication
    /// prevents cycles and infinite loops.
    async fn run_callees_bfs(
        &self,
        root_symbols: &[String],
        query: &CallGraphQuery,
    ) -> Result<Vec<Vec<ContextNode>>, DomainError> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        for sym in root_symbols {
            if visited.insert(sym.clone()) {
                queue.push_back((sym.clone(), 0));
            }
        }

        // by_depth[i] holds nodes at depth i+1
        let mut by_depth: Vec<Vec<ContextNode>> = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            let callees = self.call_graph.find_callees(&current, query).await?;
            if callees.is_empty() {
                continue;
            }

            let next_depth = depth + 1;

            while by_depth.len() < next_depth {
                by_depth.push(Vec::new());
            }

            for reference in &callees {
                let callee_sym = reference.callee_symbol().to_string();
                if visited.contains(&callee_sym) {
                    continue;
                }
                visited.insert(callee_sym.clone());
                by_depth[next_depth - 1].push(ContextNode {
                    symbol: callee_sym.clone(),
                    depth: next_depth,
                    file_path: reference.reference_file_path().to_string(),
                    line: reference.reference_line(),
                    reference_kind: reference.reference_kind().to_string(),
                    repository_id: reference.repository_id().to_string(),
                    import_alias: reference.import_alias().map(str::to_string),
                    via_symbol: Some(current.clone()),
                });
                queue.push_back((callee_sym, next_depth));
            }
        }

        Ok(by_depth)
    }
}
