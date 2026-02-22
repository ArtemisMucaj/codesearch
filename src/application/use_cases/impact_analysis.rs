use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::DomainError;

/// A single node in the impact (blast-radius) graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactNode {
    /// The affected symbol name.
    pub symbol: String,
    /// Hop distance from the root symbol (1 = direct caller, 2 = caller of caller, …).
    pub depth: usize,
    /// File where the caller symbol is declared.
    pub file_path: String,
    /// Kind of reference relationship (e.g. "call", "type_reference").
    pub reference_kind: String,
    /// Repository that contains the caller symbol.
    pub repository_id: String,
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
                let caller_sym = match reference.caller_symbol() {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                if visited.contains(&caller_sym) {
                    continue;
                }
                visited.insert(caller_sym.clone());

                by_depth[next_depth - 1].push(ImpactNode {
                    symbol: caller_sym.clone(),
                    depth: next_depth,
                    file_path: reference.caller_file_path().to_string(),
                    reference_kind: reference.reference_kind().to_string(),
                    repository_id: reference.repository_id().to_string(),
                });

                queue.push_back((caller_sym, next_depth));
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
