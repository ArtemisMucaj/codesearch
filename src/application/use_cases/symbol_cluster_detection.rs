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
//! `cluster_detection` (`Graph`, `leiden`, `kind_weight`) so the two levels stay
//! behaviourally identical and benefit from the same fixes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use tracing::{debug, warn};

use super::cluster_detection::{
    facade_split_config, kind_weight, leiden, partition_with_facade_split, Graph,
};
use crate::application::{AnalysisRepository, CallGraphUseCase, MetadataRepository};
use crate::domain::{
    community_label, namespace_scope_id, stable_community_id, CommunityMeta, DomainError,
    GraphEdge, GraphLevel, GraphNode, GraphView, SymbolCommunity, SymbolCommunityGraph,
};

/// Use case: detect communities of tightly-coupled symbols in a repository's
/// call graph and answer membership queries against them.
pub struct SymbolClusterDetectionUseCase {
    call_graph: Arc<CallGraphUseCase>,
    /// Optional persistence for detected communities. When present, detection
    /// becomes a read-through cache: stored results are served directly and
    /// fresh results are written back after computing.
    storage: Option<Arc<dyn AnalysisRepository>>,
    /// Optional namespace scope for the namespace-wide (global) symbol graph:
    /// the active namespace plus a metadata repository to enumerate its
    /// repositories. `None` for the per-repository paths, which never need it.
    namespace_scope: Option<(String, Arc<dyn MetadataRepository>)>,
}

/// The intermediate symbol graph plus the bookkeeping needed to turn a Leiden
/// partition into named, scored communities. `pub(crate)` so coupling
/// detection can analyse the identical graph.
pub(crate) struct SymbolGraph {
    /// Symbol FQN per node index (ascending, deduplicated).
    pub(crate) symbols: Vec<String>,
    /// The weighted undirected graph handed to Leiden.
    pub(crate) graph: Graph,
    /// Dominant language per symbol (first seen wins).
    language_of: HashMap<String, String>,
    /// Owning repository id per symbol, for the namespace-wide graph (first
    /// reference that mentions the symbol wins). Empty for the single-repo path,
    /// where every symbol trivially belongs to the one repo. Resolved to a
    /// display name at render time.
    repo_of: HashMap<String, String>,
    /// Distinct undirected (lo, hi, weight) edges — used as the edge count, to
    /// compute per-community cohesion, and to drive the visualization view.
    edges: Vec<(usize, usize, f64)>,
}

impl SymbolClusterDetectionUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self {
            call_graph,
            storage: None,
            namespace_scope: None,
        }
    }

    /// Attach persistent storage so detected communities are cached in the
    /// database instead of being recomputed on every query.
    pub fn with_storage(mut self, storage: Arc<dyn AnalysisRepository>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Attach the namespace scope needed by the namespace-wide symbol graph
    /// (`create_namespace_symbol_communities` / `namespace_graph_view`). Without
    /// it those methods error; the per-repository paths are unaffected.
    pub fn with_namespace_scope(
        mut self,
        namespace: String,
        metadata_repo: Arc<dyn MetadataRepository>,
    ) -> Self {
        self.namespace_scope = Some((namespace, metadata_repo));
        self
    }

    /// Repository ids in a namespace, for the namespace-wide graph. `namespace`
    /// overrides the use case's default scope, so one serve can answer for any
    /// namespace via a per-request `?namespace=` without a restart; `None` uses
    /// the default scope the use case was built with.
    async fn namespace_repository_ids(
        &self,
        namespace: Option<&str>,
    ) -> Result<(String, Vec<String>), DomainError> {
        let (default_ns, metadata_repo) = self.namespace_scope.as_ref().ok_or_else(|| {
            DomainError::storage(
                "namespace-wide symbol detection requires a namespace scope".to_string(),
            )
        })?;
        let namespace = namespace.unwrap_or(default_ns.as_str());
        let ids = metadata_repo
            .list()
            .await
            .map_err(|e| DomainError::storage(format!("Failed to list repositories: {e}")))?
            .into_iter()
            .filter(|r| r.namespace() == Some(namespace))
            .map(|r| r.id().to_string())
            .collect();
        Ok((namespace.to_string(), ids))
    }

    /// Repository id → display name for every repository in `namespace`. Used to
    /// label symbol nodes with a human, git-project name instead of a UUID.
    async fn namespace_repo_names(
        &self,
        namespace: Option<&str>,
    ) -> Result<HashMap<String, String>, DomainError> {
        let (default_ns, metadata_repo) = self.namespace_scope.as_ref().ok_or_else(|| {
            DomainError::storage(
                "namespace-wide symbol detection requires a namespace scope".to_string(),
            )
        })?;
        let namespace = namespace.unwrap_or(default_ns.as_str());
        Ok(metadata_repo
            .list()
            .await
            .map_err(|e| DomainError::storage(format!("Failed to list repositories: {e}")))?
            .into_iter()
            .filter(|r| r.namespace() == Some(namespace))
            .map(|r| (r.id().to_string(), r.name().to_string()))
            .collect())
    }

    /// Detect all symbol communities in `repository_id`, serving stored results
    /// when available and persisting freshly computed ones.
    pub async fn detect_communities(
        &self,
        repository_id: &str,
    ) -> Result<SymbolCommunityGraph, DomainError> {
        let mut scg = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let sg = self.build_symbol_graph(repository_id).await?;
                let scg = self.compute_communities(repository_id, &sg);
                self.store(&scg).await;
                scg
            }
        };
        self.apply_cached_names(&mut scg.communities).await;
        Ok(scg)
    }

    /// Fill each community's `display_name` from the persistent LLM-name cache
    /// (keyed on the stable id). Pure cache read — no LLM call — so every render
    /// path shows any name already generated. Misses stay `None`.
    async fn apply_cached_names(&self, communities: &mut [SymbolCommunity]) {
        let Some(storage) = &self.storage else {
            return;
        };
        let ids: Vec<String> = communities.iter().map(|c| c.id.clone()).collect();
        let names = match storage.get_community_names(&ids).await {
            Ok(names) => names,
            Err(e) => {
                warn!("failed to load cached community names, showing ids: {e}");
                return;
            }
        };
        for community in communities {
            if let Some(name) = names.get(&community.id) {
                community.display_name = Some(name.clone());
            }
        }
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

        // Run Leiden — or the coupling-informed façade split (god-objects like a
        // shared constants class or base exception exploded into per-community
        // façades) when it is enabled.
        let partition = match facade_split_config() {
            Some(pct) => partition_with_facade_split(&sg.symbols, &sg.edges, pct),
            None => leiden(&sg.graph),
        };
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
                    id: stable_community_id("s", members),
                    display_name: None,
                    repository_id: repository_id.to_string(),
                    dominant_language: dominant_language(members, &sg.language_of),
                    size: members.len(),
                    cohesion,
                    members: members.clone(),
                }
            })
            .collect();

        // Largest first, then stable id for a deterministic order.
        communities.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));

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
        let mut scg = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let scg = self.compute_communities(repository_id, &sg);
                self.store(&scg).await;
                scg
            }
        };
        self.apply_cached_names(&mut scg.communities).await;
        // Per-repository: every symbol trivially belongs to the one repo, so no
        // per-node repository field (empty name map → all `None`).
        Ok(Self::render_symbol_view(
            repository_id,
            &sg,
            &scg,
            &HashMap::new(),
        ))
    }

    /// Namespace-wide symbol community detection: one Leiden run over the union
    /// of every repository's call graph in the namespace, cross-repository edges
    /// included. Symbol FQNs are globally unique so nodes join across repos with
    /// no qualification (unlike the file graph). Cached per namespace under the
    /// sentinel scope id, exactly like the file-level namespace run.
    pub async fn create_namespace_symbol_communities(
        &self,
        namespace: Option<&str>,
    ) -> Result<SymbolCommunityGraph, DomainError> {
        let scope = self.namespace_scope_key(namespace).await?;
        let mut scg = match self.load_stored(&scope).await {
            Some(stored) => stored,
            None => {
                let sg = self.build_namespace_symbol_graph(namespace).await?;
                let scg = self.compute_communities(&scope, &sg);
                self.store(&scg).await;
                scg
            }
        };
        self.apply_cached_names(&mut scg.communities).await;
        Ok(scg)
    }

    /// Render-ready [`GraphView`] of the namespace-wide symbol call graph,
    /// coloured by its global Leiden communities. The counterpart of
    /// [`Self::graph_view`] for the namespace scope. `namespace` overrides the
    /// default scope for per-request use.
    pub async fn namespace_graph_view(
        &self,
        namespace: Option<&str>,
    ) -> Result<GraphView, DomainError> {
        let scope = self.namespace_scope_key(namespace).await?;
        let sg = self.build_namespace_symbol_graph(namespace).await?;
        let mut scg = match self.load_stored(&scope).await {
            Some(stored) => stored,
            None => {
                let scg = self.compute_communities(&scope, &sg);
                self.store(&scg).await;
                scg
            }
        };
        self.apply_cached_names(&mut scg.communities).await;
        // Label each symbol node with its owning repository's display name so the
        // client's repo picker/coloring show git projects, not slices of FQNs.
        let repo_names = self.namespace_repo_names(namespace).await?;
        Ok(Self::render_symbol_view(&scope, &sg, &scg, &repo_names))
    }

    /// The cache scope id for a namespace's symbol run — the same sentinel the
    /// file-level namespace run uses, so a namespace's file and symbol global
    /// analyses share the per-namespace key space (kinds keep them distinct).
    async fn namespace_scope_key(&self, namespace: Option<&str>) -> Result<String, DomainError> {
        let (namespace, _) = self.namespace_repository_ids(namespace).await?;
        Ok(namespace_scope_id(&namespace))
    }

    /// Assemble a [`GraphView`] from a symbol graph and its community partition:
    /// colour each node by the community it belongs to, size by degree. Shared
    /// by the per-repository and namespace-wide view paths.
    fn render_symbol_view(
        scope: &str,
        sg: &SymbolGraph,
        scg: &SymbolCommunityGraph,
        repo_names: &HashMap<String, String>,
    ) -> GraphView {
        // Symbol FQN → community index (position in the size-sorted list).
        let mut symbol_community: HashMap<&str, usize> = HashMap::new();
        let mut communities: Vec<CommunityMeta> = Vec::with_capacity(scg.communities.len());
        for (idx, c) in scg.communities.iter().enumerate() {
            for member in &c.members {
                symbol_community.insert(member.as_str(), idx);
            }
            communities.push(CommunityMeta {
                index: idx,
                name: community_label(&c.display_name, &c.id).to_string(),
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
                // Resolve the symbol's owning repository id to its display name.
                // Empty `repo_names` (the per-repository path) leaves this `None`,
                // so only the namespace-wide graph carries the repository field.
                repository: sg
                    .repo_of
                    .get(fqn)
                    .and_then(|id| repo_names.get(id))
                    .cloned(),
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

        GraphView {
            repository_id: scope.to_string(),
            level: GraphLevel::Symbol,
            nodes,
            edges,
            communities,
        }
    }

    /// Build the undirected, weighted symbol graph from the repository's call
    /// graph. Only symbols that participate in at least one caller→callee edge
    /// become nodes; isolated and anonymous-only symbols are dropped so the
    /// communities stay meaningful.
    pub(crate) async fn build_symbol_graph(
        &self,
        repository_id: &str,
    ) -> Result<SymbolGraph, DomainError> {
        let references = self.call_graph.find_by_repository(repository_id).await?;
        Ok(Self::symbol_graph_from_references(&references))
    }

    /// Build the **namespace-wide** symbol graph: the call graphs of every
    /// repository in the namespace, unioned into one graph.
    ///
    /// Symbols are keyed by their fully-qualified name, which is globally unique
    /// (unlike file paths, which collide across repos and must be qualified for
    /// the file graph). So a caller in one repository referencing a callee
    /// defined in another lands on the *same* node in both call graphs, and the
    /// union naturally welds the two — no qualification needed. The cross-repo
    /// edges are real: they come from `find_by_repositories` loading every
    /// repository's references together.
    pub(crate) async fn build_namespace_symbol_graph(
        &self,
        namespace: Option<&str>,
    ) -> Result<SymbolGraph, DomainError> {
        let (_, repo_ids) = self.namespace_repository_ids(namespace).await?;
        let references = self.call_graph.find_by_repositories(&repo_ids).await?;
        Ok(Self::symbol_graph_from_references(&references))
    }

    /// Fold a set of symbol references into the undirected, weighted symbol
    /// graph. Shared by the per-repository and namespace-wide builders so both
    /// aggregate edges, drop self/anonymous references, and order nodes/edges
    /// identically. The references may span multiple repositories; edges form
    /// wherever a caller and callee FQN both appear, including across repos.
    fn symbol_graph_from_references(references: &[crate::domain::SymbolReference]) -> SymbolGraph {
        // Aggregate parallel edges; collect the node set from edge endpoints.
        let mut edge_weights: HashMap<(String, String), f64> = HashMap::new();
        let mut language_of: HashMap<String, String> = HashMap::new();
        let mut repo_of: HashMap<String, String> = HashMap::new();
        let mut node_set: BTreeSet<String> = BTreeSet::new();

        for reference in references {
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

            // Attribute each symbol to a repository. The *caller* is defined in
            // the reference's repository, so that's authoritative — always record
            // it. The callee's own definition site is unknown here (it may live in
            // another repo); take the reference's repo only as a first-seen guess,
            // which a later reference where the callee is itself a caller corrects.
            let repo = reference.repository_id().to_string();
            repo_of.insert(caller.to_string(), repo.clone());
            repo_of.entry(callee.to_string()).or_insert(repo);

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
        pairs.sort_unstable_by_key(|x| x.0);

        let mut graph = Graph::new(symbols.len());
        let mut edges: Vec<(usize, usize, f64)> = Vec::with_capacity(pairs.len());
        for ((lo, hi), w) in pairs {
            graph.add_edge(lo, hi, w);
            edges.push((lo, hi, w));
        }

        SymbolGraph {
            symbols,
            graph,
            language_of,
            repo_of,
            edges,
        }
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
        .trim_end_matches(['.', '(', ')'])
        .split([':', '/', '#', '.', '\\'])
        .rfind(|s| !s.is_empty())
        .unwrap_or(symbol)
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
    fn test_find_symbol_community_prefers_exact() {
        let communities = vec![
            SymbolCommunity {
                id: "1".into(),
                display_name: None,
                repository_id: "r".into(),
                dominant_language: "rust".into(),
                size: 1,
                cohesion: 1.0,
                members: vec!["pkg/Auth#authenticate().".into()],
            },
            SymbolCommunity {
                id: "2".into(),
                display_name: None,
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
            display_name: None,
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
