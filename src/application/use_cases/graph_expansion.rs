use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::debug;

use crate::application::use_cases::pattern_utils::parse_fqn;
use crate::application::{CallGraphQuery, CallGraphUseCase, VectorRepository};
use crate::domain::{CodeChunk, DomainError, SearchQuery, SearchResult};

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
    #[tracing::instrument(skip_all, fields(seeds = seeds.len()))]
    pub async fn expand(
        &self,
        seeds: &[SearchResult],
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, DomainError> {
        let seed_symbols = Self::seed_symbols(seeds);
        if seed_symbols.is_empty() {
            return Ok(vec![]);
        }

        // Keyed by (symbol, repository) so a neighbour in another repository
        // that merely shares a seed's name is not excluded.
        let seed_set: HashSet<(&str, &str)> = seed_symbols
            .iter()
            .map(|(s, r)| (s.as_str(), r.as_str()))
            .collect();
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

        // Resolve neighbour symbols to definition chunks with one batched
        // lookup per repository, then disambiguate per symbol in memory
        // (chunks.symbol_name has no index, so per-symbol point queries each
        // cost a table scan).
        let parsed: Vec<(&str, Option<&str>)> =
            ranked.iter().map(|(symbol, _)| parse_fqn(symbol)).collect();

        let mut short_names_by_repo: HashMap<&str, Vec<&str>> = HashMap::new();
        for ((_, score), (short_name, _)) in ranked.iter().zip(&parsed) {
            if !short_name.is_empty() {
                short_names_by_repo
                    .entry(score.repository_id.as_str())
                    .or_default()
                    .push(short_name);
            }
        }

        let mut chunks_by_symbol: HashMap<(String, String), Vec<CodeChunk>> = HashMap::new();
        for (repo_id, symbols) in &short_names_by_repo {
            for chunk in self
                .vector_repo
                .find_chunks_by_symbols(repo_id, symbols)
                .await?
            {
                let Some(symbol_name) = chunk.symbol_name() else {
                    continue;
                };
                chunks_by_symbol
                    .entry((repo_id.to_string(), symbol_name.to_string()))
                    .or_default()
                    .push(chunk);
            }
        }

        let mut results: Vec<SearchResult> = Vec::new();
        let mut seen_chunks: HashSet<String> = HashSet::new();

        for ((_, score), (short_name, class_hint)) in ranked.iter().zip(&parsed) {
            if results.len() >= GRAPH_LEG_LIMIT {
                break;
            }

            let key = (score.repository_id.clone(), short_name.to_string());
            let Some(candidates) = chunks_by_symbol.get(&key) else {
                continue; // symbol has no indexed definition chunk
            };

            // Pick the definition chunk the same way find_chunk_by_symbol
            // ranks: prefer files matching the class hint, then the tightest
            // scope (smallest line span).
            let best = candidates.iter().min_by_key(|c| {
                let hint_rank = match class_hint {
                    Some(hint) if c.file_path().contains(hint) => 0u32,
                    Some(_) => 1,
                    None => 0,
                };
                (hint_rank, c.end_line().saturating_sub(c.start_line()))
            });
            let Some(chunk) = best else {
                continue;
            };

            if !query.matches(chunk) || !seen_chunks.insert(chunk.id().to_string()) {
                continue;
            }

            // Placeholder rank-derived score: RRF fusion re-scores purely by
            // list position, so only the relative order matters here.
            let rank_score = 1.0 / (results.len() as f32 + 1.0);
            results.push(SearchResult::new(chunk.clone(), rank_score));
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
        seed_set: &HashSet<(&str, &str)>,
        neighbor: &str,
        repository_id: &str,
    ) {
        if neighbor.is_empty() || seed_set.contains(&(neighbor, repository_id)) {
            return;
        }
        // Membership checks before the inserts so repeat edges (the common
        // case) allocate no keys.
        if !neighbors.contains_key(neighbor) {
            neighbors.insert(
                neighbor.to_string(),
                NeighborScore {
                    seed_count: 0,
                    edge_count: 0,
                    repository_id: repository_id.to_string(),
                },
            );
        }
        let Some(entry) = neighbors.get_mut(neighbor) else {
            return;
        };
        entry.edge_count += 1;
        if !connected.contains(neighbor) {
            connected.insert(neighbor.to_string());
            entry.seed_count += 1;
        }
    }
}
