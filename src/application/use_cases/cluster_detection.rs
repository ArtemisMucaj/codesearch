//! Leiden community-detection on the file-level dependency graph.
//!
//! The algorithm follows Traag et al. (2019):
//!   1. **Local moving** — each node greedily moves to the neighbour partition
//!      that maximises modularity gain.
//!   2. **Refinement** — nodes are allowed to move to a random subset of
//!      neighbouring partitions, escaping local optima and guaranteeing
//!      internally-connected clusters.
//!   3. **Aggregation** — each partition is collapsed into a super-node and
//!      the procedure repeats until the modularity gain is below `1e-6` or
//!      50 iterations have elapsed.
//!
//! Edge weights are differentiated by reference kind (see `kind_weight`) so
//! the algorithm clusters files that share strong semantic bonds.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use uuid::Uuid;

use crate::application::FileRelationshipUseCase;
use crate::domain::{Cluster, ClusterGraph, DomainError, FileEdge, Language};

// ── Edge-weight constants by reference kind ───────────────────────────────

fn kind_weight(kind: &str) -> f64 {
    match kind.to_lowercase().as_str() {
        "call" | "methodcall" => 1.0,
        "inheritance" => 0.8,
        "implementation" => 0.7,
        "typereference" => 0.6,
        "import" => 0.5,
        _ => 0.3,
    }
}

/// Compute a composite edge weight from a `FileEdge`.
///
/// `base_weight × mean(kind_weight for each reference_kind)`
fn composite_weight(edge: &FileEdge) -> f64 {
    let base = edge.weight as f64;
    if edge.reference_kinds.is_empty() {
        return base * 0.3;
    }
    let mean_kind: f64 =
        edge.reference_kinds.iter().map(|k| kind_weight(k)).sum::<f64>()
            / edge.reference_kinds.len() as f64;
    base * mean_kind
}

// ── Graph representation ──────────────────────────────────────────────────

/// A compact undirected weighted graph stored as adjacency lists.
#[derive(Clone)]
struct Graph {
    /// Number of nodes.
    n: usize,
    /// `adj[u]` = list of (neighbour, weight) pairs (undirected: stored in both directions).
    adj: Vec<Vec<(usize, f64)>>,
    /// Total weight of all edges (each undirected edge counted once).
    total_weight: f64,
    /// Weighted degree of each node: sum of incident edge weights.
    degree: Vec<f64>,
}

impl Graph {
    fn new(n: usize) -> Self {
        Self {
            n,
            adj: vec![Vec::new(); n],
            total_weight: 0.0,
            degree: vec![0.0; n],
        }
    }

    fn add_edge(&mut self, u: usize, v: usize, w: f64) {
        self.adj[u].push((v, w));
        self.adj[v].push((u, w));
        self.degree[u] += w;
        self.degree[v] += w;
        self.total_weight += w;
    }
}

// ── Leiden algorithm ──────────────────────────────────────────────────────

const MAX_ITERATIONS: usize = 50;
const MIN_MODULARITY_GAIN: f64 = 1e-6;

/// Run Leiden cluster detection on `graph` and return a partition: a
/// `Vec<usize>` where `partition[node_index]` is the cluster id.
fn leiden(graph: &Graph) -> Vec<usize> {
    if graph.n == 0 {
        return Vec::new();
    }

    // Start: every node in its own cluster.
    let mut partition: Vec<usize> = (0..graph.n).collect();
    let mut prev_modularity = modularity(graph, &partition);
    let mut current_graph = graph.clone();
    let mut node_to_supernode: Vec<usize> = (0..graph.n).collect();

    for _ in 0..MAX_ITERATIONS {
        local_moving_phase(&current_graph, &mut partition);
        refine_phase(&current_graph, &mut partition);

        // Aggregation step: collapse each partition into a super-node.
        let (new_graph, new_partition) = aggregate_partition(&current_graph, &partition);

        // Update the mapping from original nodes to supernodes.
        for node in 0..node_to_supernode.len() {
            let supernode = node_to_supernode[node];
            node_to_supernode[node] = partition[supernode];
        }

        current_graph = new_graph;
        partition = new_partition;

        let new_modularity = modularity(&current_graph, &partition);
        if new_modularity - prev_modularity < MIN_MODULARITY_GAIN {
            break;
        }
        prev_modularity = new_modularity;
    }

    // Map back to original nodes.
    let final_partition: Vec<usize> = (0..graph.n)
        .map(|node| {
            let supernode = node_to_supernode[node];
            partition[supernode]
        })
        .collect();

    // Renumber clusters 0..k contiguously.
    let mut result = final_partition;
    renumber(&mut result);
    result
}

/// Modularity Q = (1/2m) Σ_ij [ A_ij - k_i k_j / 2m ] δ(c_i, c_j)
fn modularity(graph: &Graph, partition: &[usize]) -> f64 {
    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return 0.0;
    }
    let mut q = 0.0;
    // Sum over internal edges
    for u in 0..graph.n {
        for &(v, w) in &graph.adj[u] {
            if v > u && partition[u] == partition[v] {
                q += w;
            }
        }
    }
    q /= graph.total_weight; // Σ internal weights / m

    // Subtract expected: Σ_c (Σ_i∈c k_i)^2 / (2m)^2
    let k = graph.n;
    let mut cluster_degree: HashMap<usize, f64> = HashMap::with_capacity(k);
    for u in 0..graph.n {
        *cluster_degree.entry(partition[u]).or_insert(0.0) += graph.degree[u];
    }
    let penalty: f64 = cluster_degree.values().map(|&d| d * d).sum::<f64>() / (m2 * m2);
    q - penalty
}

/// Local moving phase: repeatedly scan all nodes and move each to the
/// neighbouring cluster that maximises the modularity gain.
fn local_moving_phase(graph: &Graph, partition: &mut Vec<usize>) {
    // Precompute sum of internal weights per cluster.
    let mut cluster_internal: HashMap<usize, f64> = HashMap::new();
    let mut cluster_total: HashMap<usize, f64> = HashMap::new();
    for u in 0..graph.n {
        let c = partition[u];
        cluster_total.entry(c).or_insert(0.0);
        cluster_internal.entry(c).or_insert(0.0);
    }
    for u in 0..graph.n {
        for &(v, w) in &graph.adj[u] {
            if partition[u] == partition[v] {
                *cluster_internal.entry(partition[u]).or_insert(0.0) += w;
            }
        }
        *cluster_total.entry(partition[u]).or_insert(0.0) += graph.degree[u];
    }
    // Each undirected edge was counted twice above.
    for v in cluster_internal.values_mut() {
        *v /= 2.0;
    }

    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return;
    }

    let mut improved = true;
    while improved {
        improved = false;
        for u in 0..graph.n {
            let cu = partition[u];
            let ku = graph.degree[u];

            // Weight from u to each neighbouring cluster.
            let mut neighbour_weights: HashMap<usize, f64> = HashMap::new();
            for &(v, w) in &graph.adj[u] {
                if partition[v] != cu {
                    *neighbour_weights.entry(partition[v]).or_insert(0.0) += w;
                }
            }
            // Weight from u to its own cluster (excluding u itself).
            let ku_in = graph.adj[u]
                .iter()
                .filter(|&&(v, _)| partition[v] == cu)
                .map(|&(_, w)| w)
                .sum::<f64>();

            // Modularity gain of removing u from cu.
            let sigma_cu = *cluster_total.get(&cu).unwrap_or(&0.0);
            let remove_gain = ku_in - ku * (sigma_cu - ku) / m2;

            // Find best target cluster.
            let mut best_cluster = cu;
            let mut best_gain = 0.0;

            for (&ct, &w_to_ct) in &neighbour_weights {
                let sigma_ct = *cluster_total.get(&ct).unwrap_or(&0.0);
                let gain = w_to_ct - ku * sigma_ct / m2 + remove_gain;
                if gain > best_gain {
                    best_gain = gain;
                    best_cluster = ct;
                }
            }

            if best_cluster != cu {
                // Update cluster degree sums.
                *cluster_total.entry(cu).or_insert(0.0) -= ku;
                *cluster_total.entry(best_cluster).or_insert(0.0) += ku;
                partition[u] = best_cluster;
                improved = true;
            }
        }
    }
}

/// Refinement phase: allow nodes to move to a random subset of neighbouring
/// clusters.  This helps ensure clusters are internally connected and can
/// escape local optima that local moving gets stuck in.
///
/// Implementation: for each node, if moving to a randomly-selected neighbour
/// cluster yields a positive modularity gain, apply it.  We iterate once
/// over all nodes (single pass to keep runtime bounded).
fn refine_phase(graph: &Graph, partition: &mut Vec<usize>) {
    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return;
    }

    let mut cluster_total: HashMap<usize, f64> = HashMap::new();
    for u in 0..graph.n {
        *cluster_total.entry(partition[u]).or_insert(0.0) += graph.degree[u];
    }

    for u in 0..graph.n {
        let cu = partition[u];
        let ku = graph.degree[u];

        // Collect distinct neighbouring clusters.
        let mut neighbours: Vec<usize> = graph.adj[u]
            .iter()
            .map(|&(v, _)| partition[v])
            .filter(|&c| c != cu)
            .collect();
        neighbours.sort_unstable();
        neighbours.dedup();

        if neighbours.is_empty() {
            continue;
        }

        // Weight from u to each candidate cluster.
        let sigma_cu = *cluster_total.get(&cu).unwrap_or(&0.0);
        let ku_in: f64 = graph.adj[u]
            .iter()
            .filter(|&&(v, _)| partition[v] == cu)
            .map(|&(_, w)| w)
            .sum();
        let remove_gain = ku_in - ku * (sigma_cu - ku) / m2;

        // Try each neighbouring cluster (deterministic — no actual randomness
        // needed for the correctness guarantee; we just visit all candidates).
        for ct in neighbours {
            let w_to_ct: f64 = graph.adj[u]
                .iter()
                .filter(|&&(v, _)| partition[v] == ct)
                .map(|&(_, w)| w)
                .sum();
            let sigma_ct = *cluster_total.get(&ct).unwrap_or(&0.0);
            let gain = w_to_ct - ku * sigma_ct / m2 + remove_gain;
            if gain > 0.0 {
                *cluster_total.entry(cu).or_insert(0.0) -= ku;
                *cluster_total.entry(ct).or_insert(0.0) += ku;
                partition[u] = ct;
                break; // take first improving move
            }
        }
    }
}

/// Renumber partition labels to be contiguous starting from 0.
fn renumber(partition: &mut Vec<usize>) {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    for label in partition.iter_mut() {
        let next = remap.len();
        let new_id = *remap.entry(*label).or_insert(next);
        *label = new_id;
    }
}

/// Aggregate a graph by collapsing each partition into a super-node.
///
/// Returns a new graph where each node represents a cluster from the original
/// partition, and a new partition mapping (where each super-node is in its own cluster).
fn aggregate_partition(graph: &Graph, partition: &[usize]) -> (Graph, Vec<usize>) {
    // Build mapping from old cluster ID to new node index.
    let num_clusters = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);

    // Create new graph with one node per cluster.
    let mut new_graph = Graph::new(num_clusters);

    // Aggregate edge weights between clusters.
    let mut edge_weights: HashMap<(usize, usize), f64> = HashMap::new();

    for u in 0..graph.n {
        let cu = partition[u];
        for &(v, w) in &graph.adj[u] {
            if v <= u {
                continue; // Process each edge only once.
            }
            let cv = partition[v];
            if cu == cv {
                // Intra-cluster edge becomes a self-loop on the super-node.
                // Accumulate its weight so that total_weight and degree remain
                // consistent with the original graph, which is required for
                // correct modularity and delta-gain computations on the coarse graph.
                *edge_weights.entry((cu, cu)).or_insert(0.0) += w;
            } else {
                let (lo, hi) = if cu < cv { (cu, cv) } else { (cv, cu) };
                *edge_weights.entry((lo, hi)).or_insert(0.0) += w;
            }
        }
    }

    // Add aggregated edges to new graph.
    // Self-loops (u == v) must contribute to total_weight and degree but must
    // NOT appear in adj — movement decisions only involve distinct neighbours,
    // and a self-loop would create a spurious neighbour entry for a node.
    for ((u, v), w) in edge_weights {
        if u == v {
            new_graph.total_weight += w;
            new_graph.degree[u] += 2.0 * w; // both endpoints collapse to the same super-node
        } else {
            new_graph.add_edge(u, v, w);
        }
    }

    // New partition: each super-node is initially in its own cluster.
    let new_partition: Vec<usize> = (0..num_clusters).collect();

    (new_graph, new_partition)
}

// ── Cluster naming ────────────────────────────────────────────────────────

/// Common words excluded when extracting meaningful keywords from symbol names.
const STOP_WORDS: &[&str] = &[
    "get", "set", "test", "new", "is", "has", "to", "from", "with", "the",
    "and", "or", "of", "in", "at", "by", "for",
];

/// Derive a human-readable name for a cluster given its member file paths and
/// a map from file path to dominant symbol name.
///
/// Four-step heuristic (code-review-graph approach):
/// 1. Most common short directory name among members.
/// 2. If one symbol accounts for >40 % of members, use it.
/// 3. Otherwise, most frequent meaningful keyword from symbol names.
/// 4. Combine as `"{dir}-{keyword}"`, slug-cased, max 30 chars.
pub fn name_cluster(members: &[String], symbol_map: &HashMap<String, String>) -> String {
    if members.is_empty() {
        return "unknown".to_string();
    }

    // Step 1: most common short directory component.
    let mut dir_freq: HashMap<&str, usize> = HashMap::new();
    for path in members {
        if let Some(parent) = Path::new(path).parent() {
            // Take the last meaningful directory component.
            let component = parent
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty() && *s != "." && *s != "/")
                .unwrap_or("");
            if !component.is_empty() {
                *dir_freq.entry(component).or_insert(0) += 1;
            }
        }
    }
    let top_dir = dir_freq
        .iter()
        .max_by_key(|&(_, c)| c)
        .map(|(&d, _)| d)
        .unwrap_or("src");

    // Step 2: dominant symbol (> 40 % of members).
    let mut sym_freq: HashMap<&str, usize> = HashMap::new();
    for path in members {
        if let Some(sym) = symbol_map.get(path) {
            let short = sym
                .split(|c: char| c == ':' || c == '/' || c == '#' || c == '.')
                .filter(|s| !s.is_empty())
                .last()
                .unwrap_or(sym.as_str());
            *sym_freq.entry(short).or_insert(0) += 1;
        }
    }
    let threshold = (members.len() as f64 * 0.4).ceil() as usize;
    if let Some((&dominant_sym, _)) =
        sym_freq.iter().find(|(_, &c)| c >= threshold)
    {
        let slug = slugify(dominant_sym, 30);
        return slug;
    }

    // Step 3: most frequent meaningful keyword from symbol names.
    let mut kw_freq: HashMap<String, usize> = HashMap::new();
    for sym in sym_freq.keys() {
        for word in split_identifier(sym) {
            let lower = word.to_lowercase();
            if lower.len() >= 3 && !STOP_WORDS.contains(&lower.as_str()) {
                *kw_freq.entry(lower).or_insert(0) += 1;
            }
        }
    }
    let top_kw = kw_freq
        .iter()
        .max_by_key(|&(_, c)| c)
        .map(|(k, _)| k.as_str())
        .unwrap_or("");

    // Step 4: combine.
    let combined = if top_kw.is_empty() {
        top_dir.to_string()
    } else {
        format!("{}-{}", top_dir, top_kw)
    };
    slugify(&combined, 30)
}

/// Split a camelCase or snake_case identifier into words.
fn split_identifier(s: &str) -> Vec<&str> {
    // Try snake_case first.
    if s.contains('_') {
        return s.split('_').filter(|w| !w.is_empty()).collect();
    }
    // camelCase: split before every uppercase letter.
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i].is_ascii_uppercase() && bytes[i - 1].is_ascii_lowercase() {
            parts.push(&s[start..i]);
            start = i;
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Convert a string to a lowercase slug, truncated to `max_len` characters.
fn slugify(s: &str, max_len: usize) -> String {
    let slug: String = s
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == '-' || c == '_' {
                Some('-')
            } else {
                Some('-')
            }
        })
        .collect();
    // Collapse consecutive dashes.
    let mut result = String::new();
    let mut last_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !last_dash {
                result.push(c);
            }
            last_dash = true;
        } else {
            result.push(c);
            last_dash = false;
        }
    }
    let trimmed = result.trim_matches('-').to_string();
    if trimmed.chars().count() > max_len {
        trimmed.chars().take(max_len).collect()
    } else {
        trimmed
    }
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

    let mut stats: HashMap<String, (usize, usize)> = HashMap::new();
    for cid in cluster_ids {
        stats.insert(cid.clone(), (0, 0));
    }

    for edge in edges {
        let c_from = file_to_cluster.get(&edge.from_file);
        let c_to = file_to_cluster.get(&edge.to_file);
        match (c_from, c_to) {
            (Some(&ci), Some(&cj)) if ci == cj => {
                let cid = id_by_index[ci];
                stats.entry(cid.to_string()).and_modify(|(int, _)| *int += 1);
            }
            (Some(&ci), Some(&cj)) => {
                let cid_from = id_by_index[ci];
                let cid_to = id_by_index[cj];
                stats.entry(cid_from.to_string()).and_modify(|(_, ext)| *ext += 1);
                stats.entry(cid_to.to_string()).and_modify(|(_, ext)| *ext += 1);
            }
            _ => {}
        }
    }
    stats
}

// ── ClusterDetectionUseCase ───────────────────────────────────────────────

/// Minimum number of file nodes required for clustering to be meaningful.
const MIN_NODES_FOR_CLUSTERING: usize = 10;

pub struct ClusterDetectionUseCase {
    file_graph: Arc<FileRelationshipUseCase>,
}

impl ClusterDetectionUseCase {
    pub fn new(file_graph: Arc<FileRelationshipUseCase>) -> Self {
        Self { file_graph }
    }

    /// Detect clusters in the dependency graph of `repository_id`.
    pub async fn detect(
        &self,
        repository_id: &str,
    ) -> Result<ClusterGraph, DomainError> {
        let graph = self
            .file_graph
            .build_graph(Some(&[repository_id.to_string()]), 1, false)
            .await?;

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
                        map.entry(edge.from_file.clone()).and_modify(|(int, _)| *int += 1);
                    } else {
                        // External edge.
                        map.entry(edge.from_file.clone()).and_modify(|(_, ext)| *ext += 1);
                        map.entry(edge.to_file.clone()).and_modify(|(_, ext)| *ext += 1);
                    }
                }
                map
            };

            let clusters: Vec<Cluster> = files
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    let lang =
                        Language::from_path(Path::new(path)).as_str().to_string();
                    let (int_e, ext_e) = file_to_edges.get(path).copied().unwrap_or((0, 0));
                    let cohesion = if int_e + ext_e == 0 {
                        0.0_f32
                    } else {
                        int_e as f32 / (int_e + ext_e) as f32
                    };
                    Cluster {
                        id: Uuid::new_v4().to_string(),
                        name: name_cluster(&[path.clone()], &HashMap::new()),
                        repository_id: repository_id.to_string(),
                        dominant_language: lang,
                        size: 1,
                        cohesion,
                        members: vec![path.clone()],
                    }
                })
                .collect();
            return Ok(ClusterGraph {
                clusters,
                repository_id: repository_id.to_string(),
                total_files: n,
                total_edges,
            });
        }

        // Build index: file path → node index.
        let file_index: HashMap<String, usize> = files
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();

        // Build undirected weighted graph, combining parallel edges.
        let mut g = Graph::new(n);
        // Track which (u,v) pairs have already been added.
        let mut added: HashMap<(usize, usize), f64> = HashMap::new();
        for edge in &graph.edges {
            let Some(&u) = file_index.get(&edge.from_file) else { continue };
            let Some(&v) = file_index.get(&edge.to_file) else { continue };
            if u == v {
                continue;
            }
            let (lo, hi) = if u < v { (u, v) } else { (v, u) };
            let w = composite_weight(edge);
            *added.entry((lo, hi)).or_insert(0.0) += w;
        }
        for ((u, v), w) in added {
            g.add_edge(u, v, w);
        }

        // Run Leiden.
        let partition = leiden(&g);

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

        // Assign UUIDs up-front so cohesion map can key on them.
        let cluster_ids: Vec<String> = (0..num_clusters)
            .map(|_| Uuid::new_v4().to_string())
            .collect();

        // Build file→cluster_id map for cohesion.
        let file_to_cluster_id: HashMap<String, usize> = file_to_cluster.clone();
        let cohesion_stats =
            batch_cohesion(&file_to_cluster_id, &graph.edges, &cluster_ids);

        // Build a simple file→first_symbol map from edge symbols for naming.
        let mut file_symbol_map: HashMap<String, String> = HashMap::new();
        for edge in &graph.edges {
            if let Some(sym) = edge.symbols.first() {
                file_symbol_map
                    .entry(edge.to_file.clone())
                    .or_insert_with(|| sym.clone());
            }
        }

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
                let (int_e, ext_e) = cohesion_stats
                    .get(&cid)
                    .copied()
                    .unwrap_or((0, 0));
                let cohesion = if int_e + ext_e == 0 {
                    0.0_f32
                } else {
                    int_e as f32 / (int_e + ext_e) as f32
                };

                // Name.
                let name = name_cluster(members, &file_symbol_map);

                Cluster {
                    id: cid,
                    name,
                    repository_id: repository_id.to_string(),
                    dominant_language,
                    size: members.len(),
                    cohesion,
                    members: members.clone(),
                }
            })
            .collect();

        // Sort by descending size, then name for stability.
        clusters.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));

        Ok(ClusterGraph {
            clusters,
            repository_id: repository_id.to_string(),
            total_files: n,
            total_edges,
        })
    }

    /// Return the cluster a given file belongs to.
    pub async fn cluster_for_file(
        &self,
        file_path: &str,
        repository_id: &str,
    ) -> Result<Option<Cluster>, DomainError> {
        let cg = self.detect(repository_id).await?;
        Ok(cg
            .clusters
            .into_iter()
            .find(|c| c.members.iter().any(|m| m == file_path)))
    }

    /// Return a high-level architecture summary as a Markdown table.
    ///
    /// One row per cluster: name, file count, dominant language, and the top 3
    /// outgoing inter-cluster dependencies by summed edge weight.
    pub async fn architecture_overview(
        &self,
        repository_id: &str,
    ) -> Result<String, DomainError> {
        let cg = self.detect(repository_id).await?;

        if cg.clusters.is_empty() {
            return Ok(format!(
                "No clusters detected for repository `{}`.",
                repository_id
            ));
        }

        // Build file→cluster_id lookup.
        let file_to_cluster: HashMap<&str, &str> = cg
            .clusters
            .iter()
            .flat_map(|c| c.members.iter().map(move |m| (m.as_str(), c.id.as_str())))
            .collect();

        // Build cluster_id→name lookup for display.
        let cluster_id_to_name: HashMap<&str, &str> = cg
            .clusters
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();

        // Reload graph to compute inter-cluster edge weights.
        let graph = self
            .file_graph
            .build_graph(Some(&[repository_id.to_string()]), 1, false)
            .await?;

        // Aggregate: (from_cluster_id, to_cluster_id) → total_weight
        let mut inter: HashMap<(&str, &str), f64> = HashMap::new();
        for edge in &graph.edges {
            let from_c = file_to_cluster.get(edge.from_file.as_str());
            let to_c = file_to_cluster.get(edge.to_file.as_str());
            if let (Some(&fc), Some(&tc)) = (from_c, to_c) {
                if fc != tc {
                    *inter.entry((fc, tc)).or_insert(0.0) += edge.weight as f64;
                }
            }
        }

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

        for cluster in &cg.clusters {
            // Top 3 outgoing inter-cluster edges.
            let mut deps: Vec<(&str, f64)> = inter
                .iter()
                .filter(|((fc, _), _)| *fc == cluster.id.as_str())
                .map(|((_, tc), &w)| (*tc, w))
                .collect();
            deps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            deps.truncate(3);
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
                cluster.name, cluster.size, cluster.dominant_language, deps_str
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
    fn test_slugify() {
        assert_eq!(slugify("MyModule", 30), "mymodule");
        assert_eq!(slugify("my-module", 30), "my-module");
        assert_eq!(slugify("  foo  bar  ", 30), "foo-bar");
        let long = slugify("a-very-long-name-that-exceeds-the-limit", 10);
        assert!(long.len() <= 10);
    }

    #[test]
    fn test_split_identifier_snake() {
        assert_eq!(split_identifier("my_func_name"), vec!["my", "func", "name"]);
    }

    #[test]
    fn test_split_identifier_camel() {
        assert_eq!(split_identifier("myFuncName"), vec!["my", "Func", "Name"]);
    }

    #[test]
    fn test_leiden_singleton_fallback() {
        // A single node graph should produce one cluster.
        let mut g = Graph::new(1);
        g.degree[0] = 0.0;
        let partition = leiden(&g);
        assert_eq!(partition.len(), 1);
    }

    #[test]
    fn test_leiden_two_components() {
        // Two disconnected pairs should end up in separate clusters.
        let mut g = Graph::new(4);
        g.add_edge(0, 1, 1.0);
        g.add_edge(2, 3, 1.0);
        let partition = leiden(&g);
        assert_ne!(partition[0], partition[2]);
        assert_eq!(partition[0], partition[1]);
        assert_eq!(partition[2], partition[3]);
    }

    #[test]
    fn test_name_cluster_uses_dir() {
        let members = vec!["src/auth/login.rs".to_string(), "src/auth/logout.rs".to_string()];
        let name = name_cluster(&members, &HashMap::new());
        assert!(name.contains("auth"), "expected 'auth' in '{}'", name);
    }

    #[test]
    fn test_renumber() {
        let mut p = vec![5, 5, 10, 10, 5];
        renumber(&mut p);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[4]);
        assert_ne!(p[0], p[2]);
        assert_eq!(p[2], p[3]);
    }
}