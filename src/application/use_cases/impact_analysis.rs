use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::use_cases::call_graph_traversal::{
    all_paths, bfs, depth_summary, leaf_nodes, resolve_roots, trace_path, CallGraphNode, Direction,
};
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

/// Maximum number of fully-qualified symbols to resolve from a short name before
/// seeding the blast-radius BFS. Caps the ambiguity fan-out; when a name resolves
/// to more than this many FQNs the extras are dropped and the display label says
/// so, rather than silently analysing an arbitrary alphabetical slice.
const RESOLVE_SYMBOLS_LIMIT: u32 = 100;

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
    pub by_depth: Vec<Vec<CallGraphNode>>,
}

impl ImpactAnalysis {
    /// Leaf nodes: the furthest BFS-hop callers — the "entry-point" roots of
    /// each call chain. See [`leaf_nodes`].
    pub fn leaf_nodes(&self) -> Vec<&CallGraphNode> {
        leaf_nodes(&self.by_depth.iter().flatten().collect::<Vec<_>>())
    }

    /// Build the call chain for `leaf` by walking `via_symbol` back toward the
    /// root. Returns nodes in **leaf-first** order. See [`trace_path`].
    pub fn path_for_leaf<'a>(&'a self, leaf: &'a CallGraphNode) -> Vec<&'a CallGraphNode> {
        trace_path(leaf, &self.by_depth.iter().flatten().collect::<Vec<_>>())
    }

    /// Every call chain in the blast radius — one per leaf, each leaf-first.
    /// Cheaper than [`Self::path_for_leaf`] in a loop. See [`all_paths`].
    pub fn call_chains(&self) -> Vec<Vec<&CallGraphNode>> {
        all_paths(&self.by_depth.iter().flatten().collect::<Vec<_>>())
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

    /// Compute blast radius: every symbol that transitively calls `symbol`.
    ///
    /// `symbol`        – symbol name or substring to analyse (e.g. `"authenticate"`),
    ///                   or a full POSIX regex when `is_regex` is `true`.
    /// `repository_id` – optional repository filter.
    /// `is_regex`      – when `true`, `symbol` is used as-is as a regex pattern
    ///                   (no auto-wrapping). See [`resolve_roots`].
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

        let (root_symbols, root_symbol) = resolve_roots(
            &self.call_graph,
            symbol,
            &query,
            is_regex,
            RESOLVE_SYMBOLS_LIMIT,
        )
        .await?;

        let by_depth = bfs(&self.call_graph, &root_symbols, &query, Direction::Callers).await?;
        let (total_affected, max_depth_reached) = depth_summary(&by_depth);

        Ok(ImpactAnalysis {
            root_symbol,
            root_symbols,
            total_affected,
            max_depth_reached,
            by_depth,
        })
    }
}
