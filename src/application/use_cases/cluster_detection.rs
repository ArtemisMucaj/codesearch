//! Leiden community-detection on the file-level dependency graph.
//!
//! The Leiden algorithm itself — local moving, the randomized gain-weighted
//! refinement that gives it its two guarantees over Louvain, aggregation, and
//! the oversized-split / connectivity post-passes — lives in the standalone,
//! domain-agnostic [`leiden`] crate ([`leiden::partition`]). The
//! coupling-informed façade split lives in [`leiden_coupling`]
//! ([`leiden_coupling::partition_with_facade_split`]).
//!
//! What stays here is the codesearch *policy* on top of the algorithm: how a
//! `FileGraph` becomes a weighted [`leiden::Graph`] (edge weights differentiated
//! by reference kind, see [`kind_weight`]), the façade-split configuration read
//! from the environment, and everything that turns a partition into named,
//! scored clusters for the `clusters` command.
//!
//! The result is deterministic: the crate seeds its refinement RNG with a fixed
//! constant, and codesearch builds graphs from sorted edge lists, so the same
//! input always yields the same partition (cluster *membership*; the opaque
//! UUIDs assigned to each cluster are not stable and carry no ordering meaning).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use leiden::Graph;
use leiden_coupling::partition_with_facade_split;
use tracing::{debug, warn};

use crate::application::{AnalysisRepository, FileRelationshipUseCase};
use crate::domain::{
    community_label, stable_community_id, Cluster, ClusterGraph, CommunityMeta, DomainError,
    FileEdge, GraphEdge, GraphLevel, GraphNode, GraphView, Language,
};

// ── Edge-weight constants by reference kind ───────────────────────────────

/// Weight for call/method-call references — the strongest coupling signal.
const CALL_WEIGHT: f64 = 1.0;
/// Weight for inheritance relationships.
const INHERITANCE_WEIGHT: f64 = 0.8;
/// Weight for interface/trait implementation.
const IMPLEMENTATION_WEIGHT: f64 = 0.7;
/// Weight for type references (field types, return types, etc.).
const TYPEREFERENCE_WEIGHT: f64 = 0.6;
/// Weight for import/use declarations.
const IMPORT_WEIGHT: f64 = 0.5;
/// Default weight for unrecognised reference kinds.
const DEFAULT_KIND_WEIGHT: f64 = 0.3;

pub(crate) fn kind_weight(kind: &str) -> f64 {
    match kind.to_lowercase().as_str() {
        "call" | "methodcall" => CALL_WEIGHT,
        "inheritance" => INHERITANCE_WEIGHT,
        "implementation" => IMPLEMENTATION_WEIGHT,
        "typereference" => TYPEREFERENCE_WEIGHT,
        "import" => IMPORT_WEIGHT,
        _ => DEFAULT_KIND_WEIGHT,
    }
}

/// Compute a composite edge weight from a `FileEdge`.
///
/// `base_weight × mean(kind_weight for each reference_kind)`
fn composite_weight(edge: &FileEdge) -> f64 {
    let base = edge.weight as f64;
    if edge.reference_kinds.is_empty() {
        return base * DEFAULT_KIND_WEIGHT;
    }
    let mean_kind: f64 = edge
        .reference_kinds
        .iter()
        .map(|k| kind_weight(k))
        .sum::<f64>()
        / edge.reference_kinds.len() as f64;
    base * mean_kind
}

// ── Coupling-informed façade split (experimental) ─────────────────────────
//
// A god-object (a shared constants class, a base exception, a utility
// grab-bag) is a single node wired to hundreds of otherwise-unrelated nodes.
// The façade split (in the `leiden_coupling` crate) replaces each verified
// god-object coupler `H` with one **façade per neighbouring community**, so `H`
// is no longer a single vertex two communities can route a path through — the
// false glue is gone while every real dependency survives.
//
// Enabled by `CS_FACADE_SPLIT=1`; the god-object degree gate is
// `CS_FACADE_MIN_DEGREE_PCT` (percentile, default 99). OFF by default.

/// Env var (any non-empty value enables): turn on the coupling-informed façade
/// split in place of plain Leiden for cluster / symbol-community detection.
const FACADE_SPLIT_ENV: &str = "CS_FACADE_SPLIT";
/// Env var: weighted-degree percentile a verified coupler must clear to be
/// treated as a god-object worth splitting (default [`DEFAULT_FACADE_PCT`]).
const FACADE_MIN_DEGREE_PCT_ENV: &str = "CS_FACADE_MIN_DEGREE_PCT";
/// Default degree percentile gate for god-object selection.
const DEFAULT_FACADE_PCT: f64 = 99.0;

/// Whether the façade split is enabled, and with what degree gate.
pub(crate) fn facade_split_config() -> Option<f64> {
    let enabled = std::env::var(FACADE_SPLIT_ENV)
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let pct = std::env::var(FACADE_MIN_DEGREE_PCT_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|p| (0.0..=100.0).contains(p))
        .unwrap_or(DEFAULT_FACADE_PCT);
    Some(pct)
}

// ── Directory analysis (LLM naming hint) ──────────────────────────────────

/// Count how many members live under each ancestor directory.
///
/// For every member path, this walks the directory components of its parent and
/// increments the count for each ancestor prefix (`a`, `a/b`, `a/b/c`), so the
/// returned map answers "how many members share directory X" for every X. Feeds
/// the LLM naming prompt's location hints (see `community_naming`).
pub(crate) fn ancestor_dir_frequencies(members: &[String]) -> HashMap<String, usize> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for path in members {
        let parent = match Path::new(path).parent().and_then(|p| p.to_str()) {
            Some(p) if !p.is_empty() && p != "." => p,
            _ => continue,
        };
        let mut acc = String::new();
        for component in parent
            .split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
        {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(component);
            *freq.entry(acc.clone()).or_insert(0) += 1;
        }
    }
    freq
}

/// The trailing path component of a file path, used as a node's short display
/// label (the full path stays the node id).
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

// ── Cohesion computation (O(edges) batch approach) ────────────────────────

/// Compute per-cluster cohesion using the O(edges) batch approach:
/// build a `file → cluster_index` map, then walk all edges once.
///
/// Returns `HashMap<cluster_id, (internal_edges, external_edges)>`.
fn batch_cohesion(
    file_to_cluster: &HashMap<String, usize>,
    edges: &[FileEdge],
    cluster_ids: &[String],
) -> HashMap<String, (usize, usize)> {
    // cluster_index → cluster_id string
    let id_by_index: Vec<&str> = cluster_ids.iter().map(String::as_str).collect();

    let mut stats: HashMap<String, (usize, usize)> = HashMap::with_capacity(cluster_ids.len());

    for edge in edges {
        let c_from = file_to_cluster.get(&edge.from_file);
        let c_to = file_to_cluster.get(&edge.to_file);
        match (c_from, c_to) {
            (Some(&ci), Some(&cj)) if ci == cj => {
                stats.entry(id_by_index[ci].to_string()).or_insert((0, 0)).0 += 1;
            }
            (Some(&ci), Some(&cj)) => {
                stats.entry(id_by_index[ci].to_string()).or_insert((0, 0)).1 += 1;
                stats.entry(id_by_index[cj].to_string()).or_insert((0, 0)).1 += 1;
            }
            _ => {}
        }
    }
    stats
}

// ── File-graph construction ───────────────────────────────────────────────

/// Build the undirected, weighted Leiden [`Graph`] from a file-dependency
/// graph, returning the sorted node (file path) list alongside it.
///
/// Node `i` of the returned graph is `files[i]`; parallel/directional edges are
/// combined into one undirected edge whose weight sums the composite weights.
/// Shared by cluster detection and coupling detection so both always analyse
/// the identical graph.
pub(crate) fn build_file_leiden_graph(graph: &crate::domain::FileGraph) -> (Vec<String>, Graph) {
    let files: Vec<String> = {
        let mut v: Vec<String> = graph.files.iter().cloned().collect();
        v.sort();
        v
    };
    let file_index: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i))
        .collect();

    let mut g = Graph::new(files.len());
    // Track which (u,v) pairs have already been added.
    let mut added: HashMap<(usize, usize), f64> = HashMap::new();
    for edge in &graph.edges {
        let Some(&u) = file_index.get(&edge.from_file) else {
            continue;
        };
        let Some(&v) = file_index.get(&edge.to_file) else {
            continue;
        };
        if u == v {
            continue;
        }
        let (lo, hi) = if u < v { (u, v) } else { (v, u) };
        let w = composite_weight(edge);
        *added.entry((lo, hi)).or_insert(0.0) += w;
    }
    // Insert edges in a deterministic order: adjacency-list ordering feeds
    // into the clustering phases, so HashMap iteration order must not leak in.
    let mut added_edges: Vec<((usize, usize), f64)> = added.into_iter().collect();
    added_edges.sort_unstable_by_key(|&((u, v), _)| (u, v));
    for ((u, v), w) in added_edges {
        g.add_edge(u, v, w);
    }
    (files, g)
}

// ── ClusterDetectionUseCase ───────────────────────────────────────────────

/// Minimum number of file nodes required for clustering to be meaningful.
const MIN_NODES_FOR_CLUSTERING: usize = 10;

/// One aggregated inter-cluster dependency: the summed composite weight of all
/// file-level edges going from one cluster's members into another's.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModuleDependency {
    pub from_cluster_id: String,
    pub to_cluster_id: String,
    /// Sum of [`composite_weight`] over the contributing file edges.
    pub weight: f64,
}

/// Structured module map of a repository: the Leiden cluster graph plus the
/// aggregated inter-cluster dependencies, sorted by descending weight.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModuleOverview {
    pub graph: ClusterGraph,
    pub dependencies: Vec<ModuleDependency>,
}

impl ModuleOverview {
    /// Clusters ranked by an "importance" composite: `size × (1 + external
    /// dependency weight)`, where the external weight counts inter-cluster
    /// edges in both directions (what the cluster pulls in and what depends on
    /// it). This puts the load-bearing modules — large *and* heavily coupled
    /// to the rest of the codebase — first, rather than merely the largest;
    /// the `1 +` keeps fully self-contained clusters ordered by size among
    /// themselves instead of collapsing them to a shared zero score.
    pub fn clusters_by_importance(&self) -> Vec<&Cluster> {
        let mut external: HashMap<&str, f64> = HashMap::new();
        for dep in &self.dependencies {
            *external.entry(dep.from_cluster_id.as_str()).or_insert(0.0) += dep.weight;
            *external.entry(dep.to_cluster_id.as_str()).or_insert(0.0) += dep.weight;
        }
        let importance = |c: &Cluster| {
            c.size as f64 * (1.0 + external.get(c.id.as_str()).copied().unwrap_or(0.0))
        };
        let mut ranked: Vec<&Cluster> = self.graph.clusters.iter().collect();
        // Secondary key keeps the order stable when scores tie.
        ranked.sort_by(|a, b| {
            importance(b)
                .partial_cmp(&importance(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        ranked
    }
}

pub struct ClusterDetectionUseCase {
    file_graph: Arc<FileRelationshipUseCase>,
    /// Optional persistence for detected clusters. When present, detection
    /// becomes a read-through cache: stored results are served directly and
    /// fresh results are written back after computing.
    storage: Option<Arc<dyn AnalysisRepository>>,
}

impl ClusterDetectionUseCase {
    pub fn new(file_graph: Arc<FileRelationshipUseCase>) -> Self {
        Self {
            file_graph,
            storage: None,
        }
    }

    /// Attach persistent storage so detected clusters are cached in the
    /// database instead of being recomputed on every query.
    pub fn with_storage(mut self, storage: Arc<dyn AnalysisRepository>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Load the stored cluster graph, if storage is attached and has one.
    /// Storage read failures degrade to a recompute rather than failing the
    /// query.
    async fn load_stored(&self, repository_id: &str) -> Option<ClusterGraph> {
        let storage = self.storage.as_ref()?;
        match storage.load_cluster_graph(repository_id).await {
            Ok(stored) => stored,
            Err(e) => {
                warn!("Failed to load stored clusters, recomputing: {e}");
                None
            }
        }
    }

    /// Persist a freshly computed cluster graph, best-effort. Failures are
    /// expected on read-only database connections and only cost the cache.
    async fn store(&self, graph: &ClusterGraph) {
        if let Some(storage) = &self.storage {
            if let Err(e) = storage.save_cluster_graph(graph).await {
                debug!("Skipping cluster persistence: {e}");
            }
        }
    }

    /// Return the cluster graph together with the raw file-dependency graph.
    ///
    /// The dependency graph is always rebuilt (callers need it for edge-level
    /// detail); the Leiden partition is served from storage when available.
    /// Both derive deterministically from the same call-graph snapshot (stored
    /// analyses are invalidated on re-index), so they stay consistent.
    async fn clusters_and_graph(
        &self,
        repository_id: &str,
    ) -> Result<(ClusterGraph, crate::domain::FileGraph), DomainError> {
        let graph = self
            .file_graph
            .build_graph(Some(&[repository_id.to_string()]), 1, false)
            .await?;
        let mut cg = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let cg = self.compute_clusters(repository_id, &graph);
                self.store(&cg).await;
                cg
            }
        };
        self.apply_cached_names(&mut cg.clusters).await;
        Ok((cg, graph))
    }

    /// Run Leiden on a prebuilt file-dependency graph and shape the partition
    /// into named, scored clusters.
    fn compute_clusters(
        &self,
        repository_id: &str,
        graph: &crate::domain::FileGraph,
    ) -> ClusterGraph {
        let files: Vec<String> = {
            let mut v: Vec<String> = graph.files.iter().cloned().collect();
            v.sort();
            v
        };
        let n = files.len();
        let total_edges = graph.edges.len();

        // Fallback: trivial singleton clusters for small graphs.
        if n < MIN_NODES_FOR_CLUSTERING {
            // Compute cohesion for each singleton based on the graph edges.
            let file_to_edges: HashMap<String, (usize, usize)> = {
                let mut map: HashMap<String, (usize, usize)> = HashMap::new();
                for file in &files {
                    map.insert(file.clone(), (0, 0));
                }
                for edge in &graph.edges {
                    if edge.from_file == edge.to_file {
                        // Self-edge: internal to the singleton.
                        map.entry(edge.from_file.clone())
                            .and_modify(|(int, _)| *int += 1);
                    } else {
                        // External edge.
                        map.entry(edge.from_file.clone())
                            .and_modify(|(_, ext)| *ext += 1);
                        map.entry(edge.to_file.clone())
                            .and_modify(|(_, ext)| *ext += 1);
                    }
                }
                map
            };

            let clusters: Vec<Cluster> = files
                .iter()
                .map(|path| {
                    let lang = Language::from_path(Path::new(path)).as_str().to_string();
                    let (int_e, ext_e) = file_to_edges.get(path).copied().unwrap_or((0, 0));
                    let cohesion = if int_e + ext_e == 0 {
                        0.0_f32
                    } else {
                        int_e as f32 / (int_e + ext_e) as f32
                    };
                    let members = vec![path.clone()];
                    Cluster {
                        id: stable_community_id("c", &members),
                        display_name: None,
                        repository_id: repository_id.to_string(),
                        dominant_language: lang,
                        size: 1,
                        cohesion,
                        members,
                    }
                })
                .collect();
            return ClusterGraph {
                clusters,
                repository_id: repository_id.to_string(),
                total_files: n,
                total_edges,
            };
        }

        // Build the undirected weighted graph (shared with coupling detection).
        let (files, g) = build_file_leiden_graph(graph);

        // Run Leiden — or, when the coupling-informed façade split is enabled,
        // explode verified god-object couplers into per-community façades first
        // so they can no longer glue unrelated modules into one cluster.
        let partition = match facade_split_config() {
            Some(pct) => partition_with_facade_split(&files, &g.edge_list(), pct),
            None => leiden::partition(&g),
        };

        // Group files by cluster label.
        let num_clusters = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let mut members_by_cluster: Vec<Vec<String>> = vec![Vec::new(); num_clusters];
        for (idx, &label) in partition.iter().enumerate() {
            members_by_cluster[label].push(files[idx].clone());
        }
        for v in &mut members_by_cluster {
            v.sort();
        }

        // Build file→cluster_index map for cohesion computation.
        let file_to_cluster: HashMap<String, usize> = partition
            .iter()
            .enumerate()
            .map(|(file_idx, &label)| (files[file_idx].clone(), label))
            .collect();

        // Assign stable, content-addressed ids up-front so the cohesion map can
        // key on them and a cached LLM name survives recomputation (members are
        // already sorted, so the id is deterministic).
        let cluster_ids: Vec<String> = members_by_cluster
            .iter()
            .map(|members| stable_community_id("c", members))
            .collect();

        let cohesion_stats = batch_cohesion(&file_to_cluster, &graph.edges, &cluster_ids);

        let mut clusters: Vec<Cluster> = members_by_cluster
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.is_empty())
            .map(|(label, members)| {
                let cid = cluster_ids[label].clone();

                // Dominant language.
                let mut lang_freq: HashMap<&str, usize> = HashMap::new();
                for path in members {
                    let l = Language::from_path(Path::new(path));
                    *lang_freq.entry(l.as_str()).or_insert(0) += 1;
                }
                let dominant_language = lang_freq
                    .iter()
                    .max_by_key(|&(_, c)| c)
                    .map(|(&l, _)| l)
                    .unwrap_or("unknown")
                    .to_string();

                // Cohesion.
                let (int_e, ext_e) = cohesion_stats.get(&cid).copied().unwrap_or((0, 0));
                let cohesion = if int_e + ext_e == 0 {
                    0.0_f32
                } else {
                    int_e as f32 / (int_e + ext_e) as f32
                };

                Cluster {
                    id: cid,
                    display_name: None,
                    repository_id: repository_id.to_string(),
                    dominant_language,
                    size: members.len(),
                    cohesion,
                    members: members.clone(),
                }
            })
            .collect();

        // Sort by descending size, then stable id for a deterministic order.
        clusters.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));

        ClusterGraph {
            clusters,
            repository_id: repository_id.to_string(),
            total_files: n,
            total_edges,
        }
    }

    /// Detect clusters in the dependency graph of `repository_id`, serving
    /// stored results when available and persisting freshly computed ones.
    pub async fn create_clusters(&self, repository_id: &str) -> Result<ClusterGraph, DomainError> {
        let mut cg = match self.load_stored(repository_id).await {
            Some(stored) => stored,
            None => {
                let graph = self
                    .file_graph
                    .build_graph(Some(&[repository_id.to_string()]), 1, false)
                    .await?;
                let cg = self.compute_clusters(repository_id, &graph);
                self.store(&cg).await;
                cg
            }
        };
        self.apply_cached_names(&mut cg.clusters).await;
        Ok(cg)
    }

    /// Fill each cluster's `display_name` from the persistent LLM-name cache
    /// (keyed on the stable id). Pure cache read — no LLM call — so every render
    /// path can call it cheaply and show any name already generated. Misses stay
    /// `None`; the LLM fills them via [`super::CommunityNamingUseCase`].
    async fn apply_cached_names(&self, clusters: &mut [Cluster]) {
        let Some(storage) = &self.storage else {
            return;
        };
        let ids: Vec<String> = clusters.iter().map(|c| c.id.clone()).collect();
        let names = match storage.get_community_names(&ids).await {
            Ok(names) => names,
            Err(e) => {
                warn!("failed to load cached community names, showing ids: {e}");
                return;
            }
        };
        for cluster in clusters {
            if let Some(name) = names.get(&cluster.id) {
                cluster.display_name = Some(name.clone());
            }
        }
    }

    /// Build a render-ready [`GraphView`] of the file-dependency graph, with each
    /// file coloured by the Leiden cluster it belongs to.
    ///
    /// Reuses [`Self::clusters_and_graph`] so the partition, cluster
    /// names, and cohesion are identical to what the `clusters` command reports;
    /// the community index of each node is the cluster's position in the
    /// size-sorted [`ClusterGraph::clusters`] list.
    pub async fn graph_view(&self, repository_id: &str) -> Result<GraphView, DomainError> {
        let (cg, graph) = self.clusters_and_graph(repository_id).await?;

        // Nodes: every cluster member, in (cluster, member) order so the layout
        // is deterministic. Community index = position in the size-sorted list.
        let mut node_index: HashMap<&str, usize> = HashMap::new();
        let mut nodes: Vec<GraphNode> = Vec::with_capacity(cg.total_files);
        let mut communities: Vec<CommunityMeta> = Vec::with_capacity(cg.clusters.len());
        for (idx, cluster) in cg.clusters.iter().enumerate() {
            communities.push(CommunityMeta {
                index: idx,
                name: community_label(&cluster.display_name, &cluster.id).to_string(),
                size: cluster.size,
                cohesion: cluster.cohesion,
            });
            for member in &cluster.members {
                if node_index.contains_key(member.as_str()) {
                    continue;
                }
                node_index.insert(member.as_str(), nodes.len());
                nodes.push(GraphNode {
                    id: member.clone(),
                    label: basename(member),
                    community: idx,
                    degree: 0,
                    language: Language::from_path(Path::new(member)).as_str().to_string(),
                });
            }
        }

        // Edges: collapse parallel/directional file edges into undirected pairs,
        // summing the composite weight and keeping the first reference kind seen.
        let mut pair_weight: BTreeMap<(usize, usize), (f64, Option<String>)> = BTreeMap::new();
        for edge in &graph.edges {
            let (Some(&u), Some(&v)) = (
                node_index.get(edge.from_file.as_str()),
                node_index.get(edge.to_file.as_str()),
            ) else {
                continue;
            };
            if u == v {
                continue;
            }
            let key = if u < v { (u, v) } else { (v, u) };
            let entry = pair_weight.entry(key).or_insert((0.0, None));
            entry.0 += composite_weight(edge);
            if entry.1.is_none() {
                entry.1 = edge.reference_kinds.first().map(|k| k.to_lowercase());
            }
        }

        let mut edges: Vec<GraphEdge> = Vec::with_capacity(pair_weight.len());
        for ((u, v), (weight, kind)) in pair_weight {
            nodes[u].degree += 1;
            nodes[v].degree += 1;
            edges.push(GraphEdge {
                source: u,
                target: v,
                weight,
                kind,
            });
        }

        Ok(GraphView {
            repository_id: repository_id.to_string(),
            level: GraphLevel::File,
            nodes,
            edges,
            communities,
        })
    }

    /// Return the cluster a given file belongs to.
    pub async fn cluster_for_file(
        &self,
        file_path: &str,
        repository_id: &str,
    ) -> Result<Option<Cluster>, DomainError> {
        let mut cg = self.create_clusters(repository_id).await?;
        // Build a file → cluster index for O(1) lookup instead of scanning all members.
        let cluster_idx: Option<usize> = cg
            .clusters
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.members.iter().any(|m| m == file_path).then_some(i));
        Ok(cluster_idx.map(|i| cg.clusters.swap_remove(i)))
    }

    /// Return the cluster graph together with the aggregated inter-cluster
    /// dependencies — the structured form of [`Self::architecture_overview`],
    /// for callers (e.g. the repository-wide `overview` command) that render
    /// or post-process the module map themselves.
    pub async fn module_overview(
        &self,
        repository_id: &str,
    ) -> Result<ModuleOverview, DomainError> {
        let (cg, graph) = self.clusters_and_graph(repository_id).await?;

        // Build file→cluster_id lookup.
        let file_to_cluster: HashMap<&str, &str> = cg
            .clusters
            .iter()
            .flat_map(|c| c.members.iter().map(move |m| (m.as_str(), c.id.as_str())))
            .collect();

        // Aggregate: (from_cluster_id, to_cluster_id) → total composite weight.
        let mut inter: HashMap<(&str, &str), f64> = HashMap::new();
        for edge in &graph.edges {
            let from_c = file_to_cluster.get(edge.from_file.as_str());
            let to_c = file_to_cluster.get(edge.to_file.as_str());
            if let (Some(&fc), Some(&tc)) = (from_c, to_c) {
                if fc != tc {
                    *inter.entry((fc, tc)).or_insert(0.0) += composite_weight(edge);
                }
            }
        }

        let mut dependencies: Vec<ModuleDependency> = inter
            .into_iter()
            .map(|((from, to), weight)| ModuleDependency {
                from_cluster_id: from.to_string(),
                to_cluster_id: to.to_string(),
                weight,
            })
            .collect();
        // Secondary key keeps the order stable when weights tie (the map
        // iteration order above is arbitrary).
        dependencies.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    (&a.from_cluster_id, &a.to_cluster_id)
                        .cmp(&(&b.from_cluster_id, &b.to_cluster_id))
                })
        });

        Ok(ModuleOverview {
            graph: cg,
            dependencies,
        })
    }

    /// Return a high-level architecture summary as a Markdown table.
    ///
    /// One row per cluster: name, file count, dominant language, and the top 3
    /// outgoing inter-cluster dependencies by summed edge weight.
    pub async fn architecture_overview(&self, repository_id: &str) -> Result<String, DomainError> {
        let overview = self.module_overview(repository_id).await?;
        let cg = &overview.graph;

        if cg.clusters.is_empty() {
            return Ok(format!(
                "No clusters detected for repository `{}`.",
                repository_id
            ));
        }

        // Build cluster_id→label lookup for display (LLM name, else id).
        let cluster_id_to_name: HashMap<&str, &str> = cg
            .clusters
            .iter()
            .map(|c| (c.id.as_str(), community_label(&c.display_name, &c.id)))
            .collect();

        // Build table.
        let mut out = String::new();
        out.push_str("# Architecture Overview\n\n");
        out.push_str(&format!(
            "Repository `{}` — {} clusters, {} files, {} dependency edges\n\n",
            repository_id,
            cg.clusters.len(),
            cg.total_files,
            cg.total_edges
        ));
        out.push_str("| Cluster | Files | Language | Top Dependencies |\n");
        out.push_str("|---------|-------|----------|------------------|\n");

        // Load-bearing modules first (size × external coupling), matching the
        // ordering of the repository-wide `overview` command.
        for cluster in overview.clusters_by_importance() {
            // Top 3 outgoing inter-cluster edges (dependencies are pre-sorted
            // by descending weight).
            let deps: Vec<(&str, f64)> = overview
                .dependencies
                .iter()
                .filter(|d| d.from_cluster_id == cluster.id)
                .map(|d| (d.to_cluster_id.as_str(), d.weight))
                .take(3)
                .collect();
            let deps_str = if deps.is_empty() {
                "—".to_string()
            } else {
                deps.iter()
                    .map(|(cluster_id, w)| {
                        let name = cluster_id_to_name.get(cluster_id).unwrap_or(cluster_id);
                        format!("{} ({:.0})", name, w)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };

            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                community_label(&cluster.display_name, &cluster.id),
                cluster.size,
                cluster.dominant_language,
                deps_str
            ));
        }

        Ok(out)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_weight() {
        assert_eq!(kind_weight("call"), 1.0);
        assert_eq!(kind_weight("import"), 0.5);
        assert_eq!(kind_weight("unknown_kind"), 0.3);
    }

    #[test]
    fn test_clusters_by_importance_ranks_coupled_clusters_first() {
        let mk = |id: &str, size: usize| Cluster {
            id: id.to_string(),
            display_name: None,
            repository_id: "repo".to_string(),
            dominant_language: "Rust".to_string(),
            size,
            cohesion: 1.0,
            members: Vec::new(),
        };
        // "big" is the largest but fully self-contained; "hub" and "leaf" are
        // smaller but coupled to each other.
        let overview = ModuleOverview {
            graph: ClusterGraph {
                clusters: vec![mk("big", 40), mk("hub", 10), mk("leaf", 5)],
                repository_id: "repo".to_string(),
                total_files: 55,
                total_edges: 30,
            },
            dependencies: vec![
                ModuleDependency {
                    from_cluster_id: "leaf".to_string(),
                    to_cluster_id: "hub".to_string(),
                    weight: 20.0,
                },
                ModuleDependency {
                    from_cluster_id: "hub".to_string(),
                    to_cluster_id: "leaf".to_string(),
                    weight: 10.0,
                },
            ],
        };
        let ranked: Vec<&str> = overview
            .clusters_by_importance()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        // hub: 10 × (1 + 30) = 310; leaf: 5 × 31 = 155; big: 40 × 1 = 40.
        assert_eq!(ranked, vec!["hub", "leaf", "big"]);
    }

    // ── Façade split ──────────────────────────────────────────────────────
    //
    // The Leiden algorithm and the façade split live in the `leiden` /
    // `leiden-coupling` crates and carry their own unit tests. These cases
    // check the two public entry points codesearch actually calls behave as
    // this module expects.

    /// Two `size`-cliques joined by a single weak bridge edge.
    fn two_cliques(size: usize, bridge_weight: f64) -> Graph {
        let mut g = Graph::new(size * 2);
        for base in [0usize, size] {
            for i in base..base + size {
                for j in (i + 1)..base + size {
                    g.add_edge(i, j, 1.0);
                }
            }
        }
        g.add_edge(0, size, bridge_weight);
        g
    }

    /// Two 6-cliques whose *only* connection is a single hub node (node 12)
    /// wired to every node of both cliques — the canonical god-object: it
    /// couples the two blocks and its degree dwarfs every other node's.
    fn hub_joined_cliques() -> (Vec<String>, Vec<(usize, usize, f64)>) {
        let mut g = Graph::new(13);
        for base in [0usize, 6] {
            for i in base..base + 6 {
                for j in (i + 1)..base + 6 {
                    g.add_edge(i, j, 1.0);
                }
            }
        }
        for i in 0..12 {
            g.add_edge(12, i, 1.0); // hub touches both cliques
        }
        let names: Vec<String> = (0..13).map(|i| format!("src/n{i}.rs")).collect();
        (names, g.edge_list())
    }

    #[test]
    fn test_facade_split_separates_hub_joined_cliques() {
        let (names, edges) = hub_joined_cliques();

        // Degree gate at the 90th percentile admits only the hub (node 12),
        // whose degree (12) is far above the clique nodes' (5 intra + 1 hub).
        let labels = partition_with_facade_split(&names, &edges, 90.0);

        // The two cliques must now land in different clusters: with the hub
        // exploded into a per-clique façade, nothing bridges them.
        let clique_a = labels[0];
        let clique_b = labels[6];
        assert!(
            (0..6).all(|i| labels[i] == clique_a),
            "clique A not whole: {labels:?}"
        );
        assert!(
            (6..12).all(|i| labels[i] == clique_b),
            "clique B not whole: {labels:?}"
        );
        assert_ne!(
            clique_a, clique_b,
            "façade split failed to separate the cliques: {labels:?}"
        );

        // Every original node gets exactly one label (façades collapsed away).
        assert_eq!(labels.len(), names.len());
    }

    #[test]
    fn test_facade_split_deterministic() {
        let (names, edges) = hub_joined_cliques();
        assert_eq!(
            partition_with_facade_split(&names, &edges, 90.0),
            partition_with_facade_split(&names, &edges, 90.0)
        );
    }

    #[test]
    fn test_facade_split_no_god_objects_matches_plain_leiden() {
        // Two clean cliques with no hub: nothing is a coupler, so the façade
        // path must fall back to plain Leiden's partition exactly.
        let g = two_cliques(6, 0.05);
        let names: Vec<String> = (0..g.node_count())
            .map(|i| format!("src/n{i}.rs"))
            .collect();
        let mut plain = leiden::partition(&g);
        leiden::renumber(&mut plain);
        let facade = partition_with_facade_split(&names, &g.edge_list(), 99.0);
        assert_eq!(plain, facade);
    }
}
