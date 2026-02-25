use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

pub const ANONYMOUS_SYMBOL: &str = "<anonymous>";

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
    /// The immediate parent symbol in the BFS traversal (i.e. the symbol that led to this one).
    /// `None` only for the root symbol itself; always `Some` for every other node.
    pub via_symbol: Option<String>,
}

/// Full blast-radius report for a symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactAnalysis {
    /// The symbol whose change was analysed.
    pub root_symbol: String,
    /// Total number of transitively affected symbols (excluding the root).
    pub total_affected: usize,
    /// Deepest hop level reached that contained at least one result.
    pub max_depth_reached: usize,
    /// Affected symbols grouped by hop depth (index 0 = depth 1 = direct callers).
    pub by_depth: Vec<Vec<ImpactNode>>,
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
    /// `symbol`       – symbol name to analyse (e.g. `"authenticate"`)
    /// `max_depth`    – maximum BFS hops (default: 5)
    /// `repository_id` – optional repository filter
    pub async fn analyze(
        &self,
        symbol: &str,
        max_depth: usize,
        repository_id: Option<&str>,
    ) -> Result<ImpactAnalysis, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo_id) = repository_id {
            query = query.with_repository(repo_id);
        }

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(symbol.to_string());

        // (symbol, depth)
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((symbol.to_string(), 0));

        // by_depth[i] holds nodes at depth i+1
        let mut by_depth: Vec<Vec<ImpactNode>> = Vec::new();

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

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
            root_symbol: symbol.to_string(),
            total_affected,
            max_depth_reached,
            by_depth,
        })
    }
}
