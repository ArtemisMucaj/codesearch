use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::debug;

use crate::application::{CallGraphQuery, CallGraphUseCase, VectorRepository};
use crate::domain::{DomainError, SearchQuery, SearchResult};

/// Number of top-ranked results whose symbols seed the call-graph expansion.
const GRAPH_SEED_COUNT: usize = 5;

/// Maximum direct references (callers or callees) fetched per seed symbol.
const GRAPH_NEIGHBOR_FETCH_LIMIT: u32 = 25;

/// Maximum number of neighbour chunks returned by the graph leg.
const GRAPH_LEG_LIMIT: usize = 20;

/// Structural expansion of a ranked result list over the call graph.
///
/// The top hits of the semantic/BM25 legs act as *seeds*; their direct
/// callers and callees are collected from the call graph, ranked by how many
/// seeds they connect to (then by total edge count), and resolved back to
/// code chunks.  The resulting list is a third retrieval leg that surfaces
/// code which is structurally related to what the query already matched —
/// callers of a matched function, the helpers it delegates to — even when
/// that code shares no vocabulary with the query.
pub struct GraphExpansionUseCase {
    call_graph: Arc<CallGraphUseCase>,
    vector_repo: Arc<dyn VectorRepository>,
}

/// Connectivity tally for one neighbour symbol across all seeds.
struct NeighborScore {
    /// Number of distinct seeds this symbol is connected to.
    seed_count: usize,
    /// Total number of edges observed to/from this symbol.
    edge_count: usize,
    /// Repository of the first reference that produced this neighbour; used
    /// to scope the chunk lookup.
    repository_id: String,
}

impl GraphExpansionUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>, vector_repo: Arc<dyn VectorRepository>) -> Self {
        Self {
            call_graph,
            vector_repo,
        }
    }

    /// Expand `seeds` (a ranked result list) into a list of structurally
    /// related chunks, ordered by connectivity.  Returns an empty list when
    /// no seed carries a symbol name or the call graph has no edges for them.
    pub async fn expand(
        &self,
        seeds: &[SearchResult],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let seed_symbols = Self::seed_symbols(seeds);
        if seed_symbols.is_empty() {
            return Ok(vec![]);
        }

        let seed_set: HashSet<&str> = seed_symbols.iter().map(|(s, _)| s.as_str()).collect();
        let mut neighbors: HashMap<String, NeighborScore> = HashMap::new();

        for (symbol, repo_id) in &seed_symbols {
            let cg_query = CallGraphQuery::new()
                .with_repository(repo_id.clone())
                .with_limit(GRAPH_NEIGHBOR_FETCH_LIMIT);

            let (callers, callees) = tokio::join!(
                self.call_graph.find_callers(symbol, &cg_query),
                self.call_graph.find_callees(symbol, &cg_query),
            );

            // Symbols seen for *this* seed, so seed_count counts distinct seeds.
            let mut connected: HashSet<String> = HashSet::new();

            for reference in callers?.iter() {
                if let Some(caller) = reference.caller_symbol() {
                    Self::tally(
                        &mut neighbors,
                        &mut connected,
                        &seed_set,
                        caller,
                        reference.repository_id(),
                    );
                }
            }
            for reference in callees?.iter() {
                Self::tally(
                    &mut neighbors,
                    &mut connected,
                    &seed_set,
                    reference.callee_symbol(),
                    reference.repository_id(),
                );
            }
        }

        if neighbors.is_empty() {
            return Ok(vec![]);
        }

        // Rank: most seed connections first, then most edges, then name for
        // a deterministic order.
        let mut ranked: Vec<(String, NeighborScore)> = neighbors.into_iter().collect();
        ranked.sort_by(|(a_sym, a), (b_sym, b)| {
            b.seed_count
                .cmp(&a.seed_count)
                .then(b.edge_count.cmp(&a.edge_count))
                .then(a_sym.cmp(b_sym))
        });

        let mut results: Vec<SearchResult> = Vec::new();
        let mut seen_chunks: HashSet<String> = HashSet::new();

        for (symbol, score) in &ranked {
            if results.len() >= GRAPH_LEG_LIMIT {
                break;
            }

            let (short_name, class_hint) = Self::split_symbol(symbol);
            let chunk = self
                .vector_repo
                .find_chunk_by_symbol(&score.repository_id, short_name, class_hint)
                .await?;
            let Some(chunk) = chunk else {
                continue; // symbol has no indexed definition chunk
            };

            if !Self::passes_filters(query, &chunk) || !seen_chunks.insert(chunk.id().to_string()) {
                continue;
            }

            // Placeholder rank-derived score: RRF fusion re-scores purely by
            // list position, so only the relative order matters here.
            let rank_score = 1.0 / (results.len() as f32 + 1.0);
            results.push(SearchResult::new(chunk, rank_score));
        }

        debug!(
            "Graph expansion: {} seeds → {} neighbour symbols → {} chunks",
            seed_symbols.len(),
            ranked.len(),
            results.len()
        );
        Ok(results)
    }

    /// The first [`GRAPH_SEED_COUNT`] distinct symbol names among the ranked
    /// results, each paired with its repository.
    fn seed_symbols(seeds: &[SearchResult]) -> Vec<(String, String)> {
        let mut symbols: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for result in seeds {
            if symbols.len() >= GRAPH_SEED_COUNT {
                break;
            }
            let chunk = result.chunk();
            if let Some(symbol) = chunk.symbol_name() {
                if seen.insert(symbol) {
                    symbols.push((symbol.to_string(), chunk.repository_id().to_string()));
                }
            }
        }
        symbols
    }

    /// Record one edge to `neighbor` for the current seed.
    fn tally(
        neighbors: &mut HashMap<String, NeighborScore>,
        connected: &mut HashSet<String>,
        seed_set: &HashSet<&str>,
        neighbor: &str,
        repository_id: &str,
    ) {
        if neighbor.is_empty() || seed_set.contains(neighbor) {
            return;
        }
        let entry = neighbors
            .entry(neighbor.to_string())
            .or_insert_with(|| NeighborScore {
                seed_count: 0,
                edge_count: 0,
                repository_id: repository_id.to_string(),
            });
        entry.edge_count += 1;
        if connected.insert(neighbor.to_string()) {
            entry.seed_count += 1;
        }
    }

    /// Split a possibly qualified call-graph symbol into the bare name that
    /// chunk `symbol_name` columns store, plus an optional class hint.
    ///
    /// Handles the qualification styles found in the call graph:
    /// tree-sitter bare names (`foo`), `Class#method`, path-qualified SCIP
    /// symbols (`src/foo/Bar#baz().`), and dotted/namespaced names
    /// (`module.foo`, `Ns::foo`).
    fn split_symbol(symbol: &str) -> (&str, Option<&str>) {
        let trimmed = symbol.trim_end_matches(['(', ')', '.']);

        let (scope, name) = match trimmed.rsplit_once('#') {
            Some((scope, name)) => (Some(scope), name),
            None => (None, trimmed),
        };

        // Strip any remaining path / namespace qualification from the name.
        let name_start = name.rfind(['/', '.', ':']).map(|i| i + 1).unwrap_or(0);
        let short_name = &name[name_start..];

        // The class hint is the last path segment of the scope, if any.
        let class_hint = scope.map(|s| {
            let start = s.rfind(['/', '.', ':']).map(|i| i + 1).unwrap_or(0);
            &s[start..]
        });

        (
            if short_name.is_empty() {
                trimmed
            } else {
                short_name
            },
            class_hint.filter(|h| !h.is_empty()),
        )
    }

    /// Apply the search query's optional column filters to an expanded chunk,
    /// mirroring the SQL filters of the other legs.
    fn passes_filters(query: &SearchQuery, chunk: &crate::domain::CodeChunk) -> bool {
        if let Some(languages) = query.languages() {
            if !languages.iter().any(|l| l == chunk.language().as_str()) {
                return false;
            }
        }
        if let Some(node_types) = query.node_types() {
            if !node_types.iter().any(|t| t == chunk.node_type().as_str()) {
                return false;
            }
        }
        if let Some(repo_ids) = query.repository_ids() {
            if !repo_ids.iter().any(|r| r == chunk.repository_id()) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_symbol_bare_name() {
        assert_eq!(
            GraphExpansionUseCase::split_symbol("process_file"),
            ("process_file", None)
        );
    }

    #[test]
    fn split_symbol_class_method() {
        assert_eq!(
            GraphExpansionUseCase::split_symbol("Autoloader#load"),
            ("load", Some("Autoloader"))
        );
    }

    #[test]
    fn split_symbol_scip_style() {
        assert_eq!(
            GraphExpansionUseCase::split_symbol("src/foo/Bar#baz()."),
            ("baz", Some("Bar"))
        );
    }

    #[test]
    fn split_symbol_dotted() {
        assert_eq!(
            GraphExpansionUseCase::split_symbol("module.helper"),
            ("helper", None)
        );
    }

    #[test]
    fn split_symbol_rust_path() {
        assert_eq!(
            GraphExpansionUseCase::split_symbol("crate::search::run"),
            ("run", None)
        );
    }
}
