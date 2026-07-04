//! Symbol-level community detection over the call graph.
//!
//! Where [`super::cluster_detection`] clusters *files* into architectural
//! modules, this use case runs the very same Leiden algorithm one level down —
//! over the **symbol** call graph (`symbol_references`). Nodes are individual
//! symbols (functions, methods, types) and edges are caller→callee references
//! weighted by reference kind. The resulting communities are behavioural units
//! (a feature, a collaborating set of functions) that frequently cut across file
//! and even directory boundaries, which the file-level view cannot show.
//!
//! The graph primitives and the algorithm are reused verbatim from
//! `cluster_detection` (`Graph`, `leiden`, `kind_weight`, naming helpers) so the
//! two levels stay behaviourally identical and benefit from the same fixes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use tracing::{debug, warn};
use uuid::Uuid;

use super::cluster_detection::{kind_weight, leiden, slugify, split_identifier, Graph, STOP_WORDS};
use crate::application::{AnalysisRepository, CallGraphUseCase};
use crate::domain::{
    CommunityMeta, DomainError, GraphEdge, GraphLevel, GraphNode, GraphView, SymbolCommunity,
    SymbolCommunityGraph,
};

/// Maximum length of a generated community-name slug.
const NAME_MAX_LENGTH: usize = 30;
/// Minimum keyword length considered meaningful for naming.
const MIN_KEYWORD_LEN: usize = 3;

/// Use case: detect communities of tightly-coupled symbols in a repository's
/// call graph and answer membership queries against them.
pub struct SymbolClusterDetectionUseCase {
    call_graph: Arc<CallGraphUseCase>,
    /// Optional persistence for detected communities. When present, detection
    /// becomes a read-through cache: stored results are served directly and
    /// fresh results are written back after computing.
    storage: Option<Arc<dyn AnalysisRepository>>,
}

/// The intermediate symbol graph plus the bookkeeping needed to turn a Leiden
/// partition into named, scored communities.
struct SymbolGraph {
    /// Symbol FQN per node index (ascending, deduplicated).
    symbols: Vec<String>,
    /// The weighted undirected graph handed to Leiden.
    graph: Graph,
    /// Dominant language per symbol (first seen wins).
    language_of: HashMap<String, String>,
    /// Distinct undirected (lo, hi, weight) edges — used as the edge count, to
    /// compute per-community cohesion, and to drive the visualization view.
    edges: Vec<(usize, usize, f64)>,
}

impl SymbolClusterDetectionUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self {
            call_graph,
            storage: None,
        }
    }

    /// Attach persistent storage so detected communities are cached in the
    /// database instead of being recomputed on every query.
    pub fn with_storage(mut self, storage: Arc<dyn AnalysisRepository>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Detect all symbol communities in `repository_id`, serving stored results
    /// when available and persisting freshly computed ones.
    pub async fn detect_communities(
        &self,
        repository_id: &str,
    ) -> Result<SymbolCommunityGraph, DomainError> {
        if let Some(stored) = self.load_stored(repository_id).await {
            return Ok(stored);
        }
        let sg = self.build_symbol_graph(repository_id).await?;
        let scg = self.compute_communities(repository_id, &sg);
        self.store(&scg).await;
        Ok(scg)
    }

    /// Load the stored community graph, if storage is attached and has one.
    /// Storage read failures degrade to a recompute rather than failing the
    /// query.
    async fn load_stored(&self, repository_id: &str) -> Option<SymbolCommunityGraph> {
        let storage = self.storage.as_ref()?;
        match storage.load_symbol_community_graph(repository_id).await {
            Ok(stored) => stored,
            Err(e) => {
                warn!("Failed to load stored symbol communities, recomputing: {e}");
                None
            }
        }
    }

    /// Persist a freshly computed community graph, best-effort. Failures are
    /// expected on read-only database connections and only cost the cache.
    async fn store(&self, graph: &SymbolCommunityGraph) {
        if let Some(storage) = &self.storage {
            if let Err(e) = storage.save_symbol_community_graph(graph).await {
                debug!("Skipping symbol-community persistence: {e}");
            }
        }
    }

    /// Run Leiden over a prebuilt symbol graph and shape the partition into
    /// named, scored communities.
    fn compute_communities(&self, repository_id: &str, sg: &SymbolGraph) -> SymbolCommunityGraph {
        let total_symbols = sg.symbols.len();
        let total_edges = sg.edges.len();

        if total_symbols == 0 || total_edges == 0 {
            return SymbolCommunityGraph {
                communities: Vec::new(),
                repository_id: repository_id.to_string(),
                total_symbols,
                total_edges,
            };
        }

        let partition = leiden(&sg.graph);
        let num_communities = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);

        // Group member symbols by community label.
        let mut members_by_community: Vec<Vec<String>> = vec![Vec::new(); num_communities];
        for (idx, &label) in partition.iter().enumerate() {
            members_by_community[label].push(sg.symbols[idx].clone());
        }
        for members in &mut members_by_community {
            members.sort();
        }

        // Internal / external edge counts per community for cohesion.
        let mut internal: Vec<usize> = vec![0; num_communities];
        let mut external: Vec<usize> = vec![0; num_communities];
        for &(a, b, _w) in &sg.edges {
            let (ca, cb) = (partition[a], partition[b]);
            if ca == cb {
                internal[ca] += 1;
            } else {
                external[ca] += 1;
                external[cb] += 1;
            }
        }

        let mut communities: Vec<SymbolCommunity> = members_by_community
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.is_empty())
            .map(|(label, members)| {
                let cohesion = {
                    let (i, e) = (internal[label], external[label]);
                    if i + e == 0 {
                        0.0_f32
                    } else {
                        i as f32 / (i + e) as f32
                    }
                };
                SymbolCommunity {
                    id: Uuid::new_v4().to_string(),
                    name: name_symbol_community(members),
                    repository_id: repository_id.to_string(),
                    dominant_language: dominant_language(members, &sg.language_of),
                    size: members.len(),
                    cohesion,
                    members: members.clone(),
                }
            })
            .collect();

        // Largest first, then name for a stable order.
        communities.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));

        SymbolCommunityGraph {
            communities,
            repository_id: repository_id.to_string(),
            total_symbols,
            total_edges,
        }
    }

    /// Return the community that `symbol` belongs to, or `None` if the symbol is
    /// not part of any community (unknown, or isolated in the call graph).
    ///
    /// Matching prefers an exact fully-qualified hit, then a boundary suffix
    /// match (so a short `authenticate` finds `pkg/Auth#authenticate().`), then a
    /// substring match — always case-sensitive, mirroring the call-graph queries.
    pub async fn community_for_symbol(
        &self,
        symbol: &str,
        repository_id: &str,
    ) -> Result<Option<SymbolCommunity>, DomainError> {
        let graph = self.detect_communities(repository_id).await?;
        Ok(find_symbol_community(graph.communities, symbol))
    }

    /// Build a render-ready [`GraphView`] of the symbol call graph, with each
    /// symbol coloured by the Leiden community it belongs to.
    ///
    /// The community index of each node is the community's position in the
    /// size-sorted [`SymbolCommunityGraph::communities`] list, so it matches the
    /// `symbol-clusters` command's ordering, names, and cohesion.
    pub async fn graph_view(&self, repository_id: &str) -> Result<GraphView, DomainError> {
        // The node/edge view always needs the symbol graph; the community
        // assignment can come from storage. Both derive deterministically from
        // the same call-graph snapshot (stored analyses are invalidated on
        // re-index), so a stored partition stays consistent with a fresh graph.
        let sg = self.build_symbol_graph(repository_id).await?;
        let scg = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let scg = self.compute_communities(repository_id, &sg);
                self.store(&scg).await;
                scg
            }
        };

        // Symbol FQN → community index (position in the size-sorted list).
        let mut symbol_community: HashMap<&str, usize> = HashMap::new();
        let mut communities: Vec<CommunityMeta> = Vec::with_capacity(scg.communities.len());
        for (idx, c) in scg.communities.iter().enumerate() {
            for member in &c.members {
                symbol_community.insert(member.as_str(), idx);
            }
            communities.push(CommunityMeta {
                index: idx,
                name: c.name.clone(),
                size: c.size,
                cohesion: c.cohesion,
            });
        }

        let mut nodes: Vec<GraphNode> = sg
            .symbols
            .iter()
            .map(|fqn| GraphNode {
                id: fqn.clone(),
                label: short_symbol_name(fqn).to_string(),
                community: symbol_community.get(fqn.as_str()).copied().unwrap_or(0),
                degree: 0,
                language: sg
                    .language_of
                    .get(fqn)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string()),
            })
            .collect();

        let mut edges: Vec<GraphEdge> = Vec::with_capacity(sg.edges.len());
        for &(u, v, weight) in &sg.edges {
            nodes[u].degree += 1;
            nodes[v].degree += 1;
            edges.push(GraphEdge {
                source: u,
                target: v,
                weight,
                kind: None,
            });
        }

        Ok(GraphView {
            repository_id: repository_id.to_string(),
            level: GraphLevel::Symbol,
            nodes,
            edges,
            communities,
        })
    }

    /// Build the undirected, weighted symbol graph from the repository's call
    /// graph. Only symbols that participate in at least one caller→callee edge
    /// become nodes; isolated and anonymous-only symbols are dropped so the
    /// communities stay meaningful.
    async fn build_symbol_graph(&self, repository_id: &str) -> Result<SymbolGraph, DomainError> {
        let references = self.call_graph.find_by_repository(repository_id).await?;

        // Aggregate parallel edges; collect the node set from edge endpoints.
        let mut edge_weights: HashMap<(String, String), f64> = HashMap::new();
        let mut language_of: HashMap<String, String> = HashMap::new();
        let mut node_set: BTreeSet<String> = BTreeSet::new();

        for reference in &references {
            let Some(caller) = reference.caller_symbol() else {
                continue; // anonymous / top-level caller — no symbol to attribute
            };
            let callee = reference.callee_symbol();
            if caller == callee {
                continue; // self-reference / direct recursion — not a community edge
            }

            let weight = kind_weight(reference.reference_kind().as_str());
            let lang = reference.language().as_str().to_string();
            language_of
                .entry(caller.to_string())
                .or_insert_with(|| lang.clone());
            language_of.entry(callee.to_string()).or_insert(lang);

            node_set.insert(caller.to_string());
            node_set.insert(callee.to_string());

            let key = if caller < callee {
                (caller.to_string(), callee.to_string())
            } else {
                (callee.to_string(), caller.to_string())
            };
            *edge_weights.entry(key).or_insert(0.0) += weight;
        }

        let symbols: Vec<String> = node_set.into_iter().collect();
        let index_of: HashMap<&str, usize> = symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();

        // Deterministic edge insertion order.
        let mut pairs: Vec<((usize, usize), f64)> = edge_weights
            .iter()
            .filter_map(|((a, b), &w)| {
                let ia = *index_of.get(a.as_str())?;
                let ib = *index_of.get(b.as_str())?;
                let (lo, hi) = if ia < ib { (ia, ib) } else { (ib, ia) };
                Some(((lo, hi), w))
            })
            .collect();
        pairs.sort_unstable_by(|x, y| x.0.cmp(&y.0));

        let mut graph = Graph::new(symbols.len());
        let mut edges: Vec<(usize, usize, f64)> = Vec::with_capacity(pairs.len());
        for ((lo, hi), w) in pairs {
            graph.add_edge(lo, hi, w);
            edges.push((lo, hi, w));
        }

        Ok(SymbolGraph {
            symbols,
            graph,
            language_of,
            edges,
        })
    }
}

/// Pick the most common language among a community's member symbols.
fn dominant_language(members: &[String], language_of: &HashMap<String, String>) -> String {
    let mut freq: BTreeMap<&str, usize> = BTreeMap::new();
    for m in members {
        if let Some(lang) = language_of.get(m) {
            *freq.entry(lang.as_str()).or_insert(0) += 1;
        }
    }
    freq.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(lang, _)| lang.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Reduce a fully-qualified symbol to its short, undecorated trailing name,
/// e.g. `pkg/Auth#authenticate().` → `authenticate`.
fn short_symbol_name(symbol: &str) -> &str {
    symbol
        .trim_end_matches(|c: char| matches!(c, '.' | '(' | ')'))
        .split(|c: char| matches!(c, ':' | '/' | '#' | '.' | '\\'))
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or(symbol)
}

/// The `Class#method` identifier portion of a symbol — everything after the last
/// path separator, with trailing call decoration removed. This drops directory
/// and package-path prefixes (e.g. `svc/`) so keyword-based naming keys on the
/// type and member names that actually describe a community, not on the shared
/// path every member happens to live under.
fn symbol_identifier_part(symbol: &str) -> &str {
    let trimmed = symbol.trim_end_matches(|c: char| matches!(c, '.' | '(' | ')'));
    match trimmed.rsplit_once(|c: char| matches!(c, '/' | '\\')) {
        Some((_, tail)) => tail,
        None => trimmed,
    }
}

/// Derive a human-readable community name from its members' symbol names: the
/// dominant short name if one is shared by >40% of members, otherwise the most
/// frequent meaningful keyword across all member names.
fn name_symbol_community(members: &[String]) -> String {
    if members.is_empty() {
        return "unknown".to_string();
    }

    // Dominant short name (>40% of members share it).
    let mut name_freq: BTreeMap<&str, usize> = BTreeMap::new();
    for m in members {
        *name_freq.entry(short_symbol_name(m)).or_insert(0) += 1;
    }
    if let Some((&name, &count)) = name_freq
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
    {
        // Integer-safe count/len > 0.4  ≡  count * 5 > len * 2.
        if count * 5 > members.len() * 2 {
            return slugify(name, NAME_MAX_LENGTH);
        }
    }

    // Otherwise the most frequent meaningful keyword across the type and member
    // names of each symbol (e.g. `Payment` from `PaymentService#charge`), so a
    // shared concept beats any single method name.
    let mut kw_freq: BTreeMap<String, usize> = BTreeMap::new();
    for m in members {
        for segment in symbol_identifier_part(m).split(|c: char| matches!(c, ':' | '#' | '.')) {
            for word in split_identifier(segment) {
                let lower = word.to_lowercase();
                if lower.len() >= MIN_KEYWORD_LEN && !STOP_WORDS.contains(&lower.as_str()) {
                    *kw_freq.entry(lower).or_insert(0) += 1;
                }
            }
        }
    }
    let top_kw = kw_freq
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
        .unwrap_or_default();

    if top_kw.is_empty() {
        slugify(short_symbol_name(&members[0]), NAME_MAX_LENGTH)
    } else {
        slugify(&top_kw, NAME_MAX_LENGTH)
    }
}

/// Find the community containing `symbol`, preferring an exact FQN match, then a
/// boundary suffix match, then a substring match. Case-sensitive throughout.
fn find_symbol_community(
    communities: Vec<SymbolCommunity>,
    symbol: &str,
) -> Option<SymbolCommunity> {
    // Exact fully-qualified match.
    if let Some(c) = communities
        .iter()
        .find(|c| c.members.iter().any(|m| m == symbol))
    {
        return Some(c.clone());
    }
    // Boundary suffix match: the query is the trailing short name of a member.
    if let Some(c) = communities
        .iter()
        .find(|c| c.members.iter().any(|m| short_symbol_name(m) == symbol))
    {
        return Some(c.clone());
    }
    // Substring fallback.
    communities
        .into_iter()
        .find(|c| c.members.iter().any(|m| m.contains(symbol)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_symbol_name() {
        assert_eq!(
            short_symbol_name("pkg/Auth#authenticate()."),
            "authenticate"
        );
        assert_eq!(short_symbol_name("crate::module::my_func"), "my_func");
        assert_eq!(short_symbol_name("Foo\\Bar\\baz"), "baz");
        assert_eq!(short_symbol_name("plain"), "plain");
    }

    #[test]
    fn test_name_symbol_community_dominant_name() {
        let members = vec![
            "a/X#handle().".to_string(),
            "b/Y#handle().".to_string(),
            "c/Z#handle().".to_string(),
        ];
        assert_eq!(name_symbol_community(&members), "handle");
    }

    #[test]
    fn test_name_symbol_community_keyword() {
        let members = vec![
            "svc/PaymentService#charge().".to_string(),
            "svc/PaymentGateway#refund().".to_string(),
            "svc/PaymentRepo#save().".to_string(),
        ];
        // No single short name dominates, but "payment" is the shared keyword.
        assert_eq!(name_symbol_community(&members), "payment");
    }

    #[test]
    fn test_find_symbol_community_prefers_exact() {
        let communities = vec![
            SymbolCommunity {
                id: "1".into(),
                name: "a".into(),
                repository_id: "r".into(),
                dominant_language: "rust".into(),
                size: 1,
                cohesion: 1.0,
                members: vec!["pkg/Auth#authenticate().".into()],
            },
            SymbolCommunity {
                id: "2".into(),
                name: "b".into(),
                repository_id: "r".into(),
                dominant_language: "rust".into(),
                size: 1,
                cohesion: 1.0,
                members: vec!["other/authenticate_helper".into()],
            },
        ];
        let hit = find_symbol_community(communities, "pkg/Auth#authenticate().").unwrap();
        assert_eq!(hit.id, "1");
    }

    #[test]
    fn test_find_symbol_community_suffix() {
        let communities = vec![SymbolCommunity {
            id: "1".into(),
            name: "a".into(),
            repository_id: "r".into(),
            dominant_language: "rust".into(),
            size: 1,
            cohesion: 1.0,
            members: vec!["pkg/Auth#authenticate().".into()],
        }];
        let hit = find_symbol_community(communities, "authenticate").unwrap();
        assert_eq!(hit.id, "1");
    }
}
