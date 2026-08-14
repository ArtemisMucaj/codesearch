use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::use_cases::call_graph_traversal::{
    all_paths, bfs, depth_summary, leaf_nodes, resolve_roots, trace_path, CallGraphNode, Direction,
};
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

/// Depth-grouped BFS view of a symbol's call-graph relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolContext {
    /// Display label for the analysed symbol.
    pub symbol: String,
    /// The fully-qualified symbol names used as BFS roots.
    pub root_symbols: Vec<String>,
    /// Callers BFS: index 0 = depth 1 = direct callers.
    pub callers_by_depth: Vec<Vec<CallGraphNode>>,
    /// Total number of transitively calling symbols (excluding the root).
    pub total_callers: usize,
    /// Deepest hop level reached that contained at least one caller.
    pub max_caller_depth: usize,
    /// Callees BFS: index 0 = depth 1 = direct callees.
    pub callees_by_depth: Vec<Vec<CallGraphNode>>,
    /// Total number of transitively called symbols (excluding the root).
    pub total_callees: usize,
    /// Deepest hop level reached that contained at least one callee.
    pub max_callee_depth: usize,
    /// `false` when the symbol could not be resolved against the call graph.
    /// When `false`, empty caller/callee lists mean "not indexed", NOT "no
    /// callers or callees".
    pub resolved: bool,
}

impl SymbolContext {
    /// The top-most entry points of the callers walk: caller nodes that
    /// nothing else calls. See [`leaf_nodes`].
    pub fn caller_leaves(&self) -> Vec<&CallGraphNode> {
        leaf_nodes(&self.callers_by_depth.iter().flatten().collect::<Vec<_>>())
    }

    /// Build the caller chain for `leaf`, **leaf-first**: index 0 is the
    /// top-most caller, the last entry is a direct caller of the queried
    /// symbol. See [`trace_path`].
    pub fn path_for_leaf<'a>(&'a self, leaf: &'a CallGraphNode) -> Vec<&'a CallGraphNode> {
        trace_path(
            leaf,
            &self.callers_by_depth.iter().flatten().collect::<Vec<_>>(),
        )
    }

    /// Every caller chain — one per entry point, each leaf-first. Cheaper than
    /// [`Self::path_for_leaf`] in a loop. See [`all_paths`].
    pub fn caller_chains(&self) -> Vec<Vec<&CallGraphNode>> {
        all_paths(&self.callers_by_depth.iter().flatten().collect::<Vec<_>>())
    }

    /// Map each parent symbol to its direct callee nodes, so a renderer can
    /// walk the callee subtree hanging off any symbol in the flow.
    pub fn callee_children(&self) -> HashMap<String, Vec<&CallGraphNode>> {
        let mut map: HashMap<String, Vec<&CallGraphNode>> = HashMap::new();
        for node in self.callees_by_depth.iter().flatten() {
            let key = node
                .via_symbol
                .as_deref()
                .unwrap_or(&self.symbol)
                .to_owned();
            map.entry(key).or_default().push(node);
        }

        // For multi-root-symbol queries, `symbol` is a display label like
        // "authenticate (3 symbols)". Depth-1 callee nodes point back at the
        // real FQN (e.g. "MyModule::authenticate"), not the label, so add a
        // synthetic entry aggregating them under the label — renderers can
        // then always start the subtree walk from `symbol`.
        let is_label = !self.root_symbols.iter().any(|r| r == &self.symbol);
        let depth1_nodes: Vec<&CallGraphNode> = self
            .callees_by_depth
            .first()
            .map(|d| d.iter().collect())
            .unwrap_or_default();
        if is_label && !depth1_nodes.is_empty() {
            map.insert(self.symbol.clone(), depth1_nodes);
        }
        map
    }
}

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
    ///                   (no auto-wrapping). See [`resolve_roots`].
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

        let (root_symbols, display_symbol, resolved) =
            resolve_roots(&self.call_graph, symbol, &query, is_regex).await?;

        // Run both BFS passes in parallel.
        let (callers, callees) = tokio::join!(
            bfs(&self.call_graph, &root_symbols, &query, Direction::Callers),
            bfs(&self.call_graph, &root_symbols, &query, Direction::Callees),
        );
        let callers_by_depth = callers?;
        let callees_by_depth = callees?;

        let (total_callers, max_caller_depth) = depth_summary(&callers_by_depth);
        let (total_callees, max_callee_depth) = depth_summary(&callees_by_depth);

        Ok(SymbolContext {
            symbol: display_symbol,
            root_symbols,
            callers_by_depth,
            total_callers,
            max_caller_depth,
            callees_by_depth,
            total_callees,
            max_callee_depth,
            resolved,
        })
    }
}
