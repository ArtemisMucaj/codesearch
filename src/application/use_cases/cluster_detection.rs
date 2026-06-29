//! Leiden community-detection on the file-level dependency graph.
//!
//! The algorithm follows Traag et al. (2019):
//!   1. **Local moving** — each node greedily moves to the neighbour partition
//!      that maximises modularity gain.
//!   2. **Refinement** — each community is rebuilt from singletons and its nodes
//!      re-merged into well-connected sub-communities by a randomized,
//!      gain-weighted pass (the step that makes this Leiden, not Louvain).
//!   3. **Aggregation** — the *refined* partition is collapsed into super-nodes,
//!      each seeded with its pre-refinement community, and the procedure repeats
//!      until the modularity gain is below `1e-6` or 50 iterations have elapsed.
//!
//! The refinement step ([`refine_partition`]) is the real thing: it rebuilds
//! each community from singletons, re-merging nodes into well-connected
//! sub-communities via a randomized, gain-weighted choice. This is what gives
//! Leiden its two guarantees over Louvain — every community is internally
//! connected, and the stochastic merges escape the local optima plain
//! local-moving freezes into. Two post-passes then run for robustness:
//! [`split_oversized`] subdivides any community that grows to dominate the graph
//! (so one mega-cluster cannot swallow the codebase), and
//! [`enforce_connectivity`] re-asserts the connectivity guarantee as a final
//! safety net.
//!
//! The result is deterministic despite the randomness: the refinement RNG is
//! seeded with a fixed constant ([`LEIDEN_SEED`]), candidate communities are
//! visited in a stable order, and graphs are built from sorted edge lists, so
//! the same input always yields the same partition (cluster *membership*; the
//! opaque UUIDs assigned to each cluster are not stable and carry no ordering
//! meaning).
//!
//! Edge weights are differentiated by reference kind (see [`kind_weight`]) so
//! the algorithm clusters nodes that share strong semantic bonds. The graph
//! primitives ([`Graph`], [`leiden`]) are `pub(crate)` so symbol-level community
//! detection can reuse the exact same algorithm.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use uuid::Uuid;

use crate::application::FileRelationshipUseCase;
use crate::domain::{Cluster, ClusterGraph, DomainError, FileEdge, Language};

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

/// Maximum character length for a generated cluster-name slug.
const SLUG_MAX_LENGTH: usize = 30;

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

// ── Graph representation ──────────────────────────────────────────────────

/// A compact undirected weighted graph stored as adjacency lists.
///
/// Exposed at `pub(crate)` so other use cases (e.g. symbol-level community
/// detection) can build a graph and run [`leiden`] on it without duplicating the
/// algorithm.
#[derive(Clone)]
pub(crate) struct Graph {
    /// Number of nodes.
    n: usize,
    /// `adj[u]` = list of (neighbour, weight) pairs (undirected: stored in both directions).
    adj: Vec<Vec<(usize, f64)>>,
    /// Total weight of all edges (each undirected edge counted once), including self-loops.
    total_weight: f64,
    /// Weighted degree of each node: sum of incident edge weights (a self-loop
    /// contributes twice, as it touches the node at both ends).
    degree: Vec<f64>,
    /// Per-node self-loop weight, accumulated during graph aggregation.
    ///
    /// Self-loops are not stored in `adj` (they would create spurious
    /// neighbours), but their mass is internal to whichever community the node
    /// belongs to and must be included in the internal-edge term of
    /// [`modularity`]. Tracking it per node (rather than as one scalar) lets the
    /// multi-level Leiden recursion carry intra-community mass forward correctly
    /// across successive aggregations.
    self_loops: Vec<f64>,
}

impl Graph {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            n,
            adj: vec![Vec::new(); n],
            total_weight: 0.0,
            degree: vec![0.0; n],
            self_loops: vec![0.0; n],
        }
    }

    pub(crate) fn add_edge(&mut self, u: usize, v: usize, w: f64) {
        self.adj[u].push((v, w));
        self.adj[v].push((u, w));
        self.degree[u] += w;
        self.degree[v] += w;
        self.total_weight += w;
    }

    /// Add `w` of self-loop mass to `node`. A self-loop touches the node twice,
    /// so it adds `2w` to the weighted degree but `w` to the total edge weight.
    fn add_self_loop(&mut self, node: usize, w: f64) {
        if w == 0.0 {
            return;
        }
        self.self_loops[node] += w;
        self.degree[node] += 2.0 * w;
        self.total_weight += w;
    }

    /// Total self-loop (intra-community) mass across all nodes.
    fn self_loop_total(&self) -> f64 {
        self.self_loops.iter().sum()
    }
}

// ── Deterministic PRNG ────────────────────────────────────────────────────

/// Fixed seed for the refinement RNG. Leiden's refinement is stochastic by
/// design (that randomness is what lets it escape the local optima Louvain gets
/// stuck in); seeding it with a constant keeps the result reproducible across
/// runs and processes while preserving the exploration.
const LEIDEN_SEED: u64 = 0x5EED_1DEA_C0DE_F00D;

/// Theta controls how sharply refinement prefers higher-gain merges: smaller →
/// greedier, larger → more uniform exploration. Mid-range keeps some stochastic
/// exploration without diluting clearly-better merges.
const REFINE_THETA: f64 = 0.05;

/// Minimal self-contained SplitMix64 PRNG — avoids pulling in the `rand` crate
/// for the handful of values the refinement needs.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f64 in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        // 53-bit mantissa for a uniform double.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// In-place Fisher–Yates shuffle using `rng`.
fn shuffle<T>(items: &mut [T], rng: &mut SplitMix64) {
    for i in (1..items.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

// ── Leiden algorithm ──────────────────────────────────────────────────────

const MAX_ITERATIONS: usize = 50;
const MIN_MODULARITY_GAIN: f64 = 1e-6;

/// Communities larger than this fraction of the graph are subdivided by
/// [`split_oversized`] so a single mega-cluster cannot dominate the output.
const MAX_COMMUNITY_FRACTION: f64 = 0.25;
/// A community is only considered for [`split_oversized`] when it has at least
/// this many nodes — below it the size dominance is not meaningful.
const MIN_SPLIT_SIZE: usize = 10;

/// Run Leiden cluster detection on `graph` and return a partition: a
/// `Vec<usize>` where `partition[node_index]` is the cluster id.
///
/// This is the Leiden core ([`leiden_core`]) followed by the two
/// guarantee-enforcing post-passes ([`split_oversized`] then
/// [`enforce_connectivity`]); labels are renumbered contiguously at the end.
pub(crate) fn leiden(graph: &Graph) -> Vec<usize> {
    if graph.n == 0 {
        return Vec::new();
    }

    let mut result = leiden_core(graph);
    // Split any community that dominates the graph (uses the bare core on the
    // induced subgraph, so this does not recurse through the post-passes).
    split_oversized(graph, &mut result);
    // Belt-and-braces: true Leiden refinement already yields connected
    // communities, but the oversized split and any future change could not, so
    // re-assert the guarantee as the final step.
    enforce_connectivity(graph, &mut result);
    renumber(&mut result);
    result
}

/// The Leiden core (Traag et al. 2019): repeatedly (1) move nodes to the
/// best neighbouring community, (2) **refine** each community into
/// well-connected sub-communities via a randomized, gain-weighted pass, then
/// (3) aggregate the graph using the *refined* partition while seeding the next
/// level from the *unrefined* community of each node. The refinement is what
/// separates Leiden from Louvain: it guarantees every community is internally
/// connected and lets the search escape the local optima plain local-moving
/// settles into.
///
/// Returns a partition mapped back to the original nodes (not renumbered, no
/// post-passes — that is [`leiden`]'s job, kept separate so [`split_oversized`]
/// can re-cluster a subgraph without recursing through the post-passes).
fn leiden_core(graph: &Graph) -> Vec<usize> {
    if graph.n == 0 {
        return Vec::new();
    }

    let mut rng = SplitMix64::new(LEIDEN_SEED);
    let mut current = graph.clone();
    // Partition `p` over the current (aggregated) graph's nodes.
    let mut partition: Vec<usize> = (0..current.n).collect();
    // Map every original node to its node index in `current`.
    let mut node_to_super: Vec<usize> = (0..graph.n).collect();
    let mut prev_modularity = f64::NEG_INFINITY;

    for _ in 0..MAX_ITERATIONS {
        local_moving_phase(&current, &mut partition);
        renumber(&mut partition);

        let num_communities = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let q = modularity(&current, &partition);

        // Nothing left to aggregate (every node already its own community) or no
        // meaningful modularity improvement: stop with the current partition.
        if num_communities >= current.n || q - prev_modularity < MIN_MODULARITY_GAIN {
            break;
        }
        prev_modularity = q;

        // Refine each community into well-connected sub-communities.
        let mut refined = refine_partition(&current, &partition, &mut rng);
        renumber(&mut refined);

        // Aggregate by the refined partition. The renumbered `refined` vector is
        // itself the current-node → super-node map (sub-community i becomes
        // super-node i), so there is no separate mapping to return.
        let aggregated = aggregate_by(&current, &refined);

        // Seed the next level's partition from the *unrefined* community: every
        // refined sub-community (now a super-node) inherits the community it was
        // refined out of, so local moving resumes from the coarse structure.
        let mut next_partition = vec![0usize; aggregated.n];
        for node in 0..current.n {
            next_partition[refined[node]] = partition[node];
        }

        // Compose the original→super mapping through this aggregation.
        for slot in node_to_super.iter_mut() {
            *slot = refined[*slot];
        }

        current = aggregated;
        partition = next_partition;
    }

    (0..graph.n)
        .map(|node| partition[node_to_super[node]])
        .collect()
}

/// Group node indices by their partition label, in ascending (label, node)
/// order. The deterministic ordering is what lets the post-passes split
/// communities the same way on every run.
fn group_by_label(partition: &[usize]) -> BTreeMap<usize, Vec<usize>> {
    let mut by_label: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (node, &label) in partition.iter().enumerate() {
        by_label.entry(label).or_default().push(node);
    }
    by_label
}

/// First label not currently in use — the starting point for handing out fresh
/// labels to the pieces a post-pass splits off.
fn first_free_label(partition: &[usize]) -> usize {
    partition.iter().copied().max().unwrap_or(0) + 1
}

/// Enforce Leiden's defining guarantee: every community is a single connected
/// component of the induced subgraph. Any community that is split across two or
/// more components (which the Louvain-style moving/refinement passes can
/// produce) is broken apart — the first component keeps the original label and
/// each subsequent component receives a fresh label.
///
/// Deterministic: communities are visited in ascending label order and nodes in
/// ascending index order, so the same partition always splits the same way.
fn enforce_connectivity(graph: &Graph, partition: &mut [usize]) {
    if graph.n == 0 {
        return;
    }
    let mut next_label = first_free_label(partition);

    for (_label, nodes) in group_by_label(partition) {
        let members: HashSet<usize> = nodes.iter().copied().collect();
        let mut visited: HashSet<usize> = HashSet::new();
        let mut first_component = true;

        for &start in &nodes {
            if !visited.insert(start) {
                continue;
            }
            // Collect the connected component containing `start`, restricted to
            // nodes that share this community.
            let mut component = vec![start];
            let mut stack = vec![start];
            while let Some(u) = stack.pop() {
                for &(v, _) in &graph.adj[u] {
                    if members.contains(&v) && visited.insert(v) {
                        component.push(v);
                        stack.push(v);
                    }
                }
            }

            if first_component {
                // Leave the original label in place for the first component.
                first_component = false;
            } else {
                let label = next_label;
                next_label += 1;
                for node in component {
                    partition[node] = label;
                }
            }
        }
    }
}

/// Subdivide any community whose size exceeds [`MAX_COMMUNITY_FRACTION`] of the
/// graph (and is at least [`MIN_SPLIT_SIZE`] nodes) by re-running [`leiden_core`]
/// on its induced subgraph. The first resulting sub-community keeps the original
/// label; the rest receive fresh labels. Single-level only: if the subgraph is
/// indivisible the community is left as-is.
fn split_oversized(graph: &Graph, partition: &mut [usize]) {
    if graph.n == 0 {
        return;
    }
    let max_size = (graph.n as f64 * MAX_COMMUNITY_FRACTION).ceil() as usize;
    let mut next_label = first_free_label(partition);

    for (_label, nodes) in group_by_label(partition) {
        if nodes.len() < MIN_SPLIT_SIZE || nodes.len() <= max_size {
            continue;
        }

        // Build the induced subgraph: global node id → local index (nodes are
        // already in ascending order, keeping local indices deterministic).
        let local_of: HashMap<usize, usize> =
            nodes.iter().enumerate().map(|(i, &g)| (g, i)).collect();
        let mut sub = Graph::new(nodes.len());
        for &gu in &nodes {
            let lu = local_of[&gu];
            for &(gv, w) in &graph.adj[gu] {
                // Add each intra-community edge once (gu < gv).
                if gu < gv {
                    if let Some(&lv) = local_of.get(&gv) {
                        sub.add_edge(lu, lv, w);
                    }
                }
            }
        }

        let mut sub_partition = leiden_core(&sub);
        renumber(&mut sub_partition);
        let sub_clusters = sub_partition
            .iter()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        if sub_clusters <= 1 {
            // Indivisible — leave the community intact.
            continue;
        }

        // Sub-cluster 0 keeps the original label; the rest get fresh labels.
        for (i, &gnode) in nodes.iter().enumerate() {
            let sc = sub_partition[i];
            if sc != 0 {
                partition[gnode] = next_label + sc - 1;
            }
        }
        next_label += sub_clusters - 1;
    }
}

/// Modularity Q = (1/2m) Σ_ij [ A_ij - k_i k_j / 2m ] δ(c_i, c_j)
fn modularity(graph: &Graph, partition: &[usize]) -> f64 {
    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return 0.0;
    }
    let mut q = 0.0;
    for u in 0..graph.n {
        for &(v, w) in &graph.adj[u] {
            if v > u && partition[u] == partition[v] {
                q += w;
            }
        }
    }
    // Self-loop mass was collapsed from intra-cluster edges during aggregation;
    // it is always internal to a node's own community, so it is counted
    // unconditionally alongside the intra-community adj edges.
    q += graph.self_loop_total();
    q /= graph.total_weight;

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
    let mut cluster_total: HashMap<usize, f64> = HashMap::new();
    for u in 0..graph.n {
        *cluster_total.entry(partition[u]).or_insert(0.0) += graph.degree[u];
    }

    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return;
    }

    let mut improved = true;
    let mut iters = 0usize;
    while improved && iters < MAX_ITERATIONS {
        improved = false;
        iters += 1;
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

            // Find best target cluster. Iterate candidates in ascending cluster
            // id (not HashMap order) so that ties — equal modularity gain — are
            // broken deterministically; Rust's HashMap reseeds every process, so
            // iterating it directly would make the final partition vary run to
            // run on identical input.
            let mut candidates: Vec<(usize, f64)> =
                neighbour_weights.iter().map(|(&ct, &w)| (ct, w)).collect();
            candidates.sort_unstable_by_key(|&(ct, _)| ct);

            let mut best_cluster = cu;
            let mut best_gain = 0.0;

            for (ct, w_to_ct) in candidates {
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

/// Leiden refinement: within each community of `community` (the partition
/// produced by local moving), break the community back into singletons and
/// re-merge nodes into well-connected sub-communities.
///
/// Each still-isolated node is offered the neighbouring sub-communities **inside
/// its own community** whose modularity gain is non-negative, and is merged into
/// one chosen stochastically with probability proportional to `exp(gain / θ)`.
/// Two properties fall out of this:
/// * **Connectivity** — a sub-community only ever grows by absorbing a node that
///   has an edge into it, so every resulting sub-community is connected.
/// * **Escape from local optima** — the randomized, gain-weighted choice lets
///   Leiden split communities Louvain would have frozen, the defect this whole
///   change is about.
///
/// Returns the refined sub-community label per node (not yet renumbered).
fn refine_partition(graph: &Graph, community: &[usize], rng: &mut SplitMix64) -> Vec<usize> {
    let n = graph.n;
    // Every node starts in its own singleton sub-community (id == node index).
    let mut refined: Vec<usize> = (0..n).collect();
    let m2 = 2.0 * graph.total_weight;
    if m2 == 0.0 {
        return refined;
    }
    // Weighted-degree sum and node count of each refined sub-community.
    let mut sub_degree: Vec<f64> = graph.degree.clone();
    let mut sub_size: Vec<usize> = vec![1; n];

    let mut order: Vec<usize> = (0..n).collect();
    shuffle(&mut order, rng);

    for &v in &order {
        // Only nodes still alone in their sub-community may be merged — this is
        // what keeps refined sub-communities well-connected.
        if sub_size[refined[v]] != 1 {
            continue;
        }
        let cv = community[v];
        let kv = graph.degree[v];

        // Edge weight from v to each candidate sub-community within v's community.
        let mut weight_to: HashMap<usize, f64> = HashMap::new();
        for &(u, w) in &graph.adj[v] {
            if community[u] == cv && refined[u] != refined[v] {
                *weight_to.entry(refined[u]).or_insert(0.0) += w;
            }
        }
        if weight_to.is_empty() {
            continue;
        }

        // Candidate sub-communities with non-negative modularity gain, visited in
        // ascending id so the (seeded) sampling below is reproducible.
        let mut candidates: Vec<(usize, f64)> = weight_to.into_iter().collect();
        candidates.sort_unstable_by_key(|&(c, _)| c);
        let mut gains: Vec<(usize, f64)> = Vec::new();
        for (c, w_to_c) in candidates {
            // Merging a singleton into c: gain = w_to_c - k_v * Σ_c / 2m
            // (the singleton has no internal mass, so its removal cost is 0).
            let gain = w_to_c - kv * sub_degree[c] / m2;
            if gain >= 0.0 {
                gains.push((c, gain));
            }
        }
        if gains.is_empty() {
            continue;
        }

        // Sample a target ~ exp(gain / θ), shifted by the max gain for numerical
        // stability.
        let max_gain = gains.iter().map(|&(_, g)| g).fold(f64::MIN, f64::max);
        let weights: Vec<f64> = gains
            .iter()
            .map(|&(_, g)| ((g - max_gain) / REFINE_THETA).exp())
            .collect();
        let total: f64 = weights.iter().sum();
        let threshold = rng.next_f64() * total;
        let mut acc = 0.0;
        let mut chosen = gains[0].0;
        for (idx, &w) in weights.iter().enumerate() {
            acc += w;
            if threshold <= acc {
                chosen = gains[idx].0;
                break;
            }
        }

        // Merge v into the chosen sub-community.
        let old = refined[v];
        refined[v] = chosen;
        sub_degree[chosen] += kv;
        sub_degree[old] -= kv;
        sub_size[chosen] += 1;
        sub_size[old] -= 1;
    }

    refined
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

/// Aggregate `graph` by collapsing each group in `membership` (assumed
/// contiguous `0..k`) into a single super-node, returning the aggregated graph.
///
/// `membership` doubles as the node → super-node map (node `i` collapses into
/// super-node `membership[i]`), so it is not returned. Intra-group edges and
/// each node's existing self-loop mass are carried forward as the super-node's
/// self-loop, so total edge weight is conserved across aggregation levels.
fn aggregate_by(graph: &Graph, membership: &[usize]) -> Graph {
    let num = membership.iter().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut new_graph = Graph::new(num);

    let mut inter: HashMap<(usize, usize), f64> = HashMap::new();
    let mut self_mass: Vec<f64> = vec![0.0; num];

    for u in 0..graph.n {
        let cu = membership[u];
        // Carry this node's own self-loop mass into its super-node.
        self_mass[cu] += graph.self_loops[u];
        for &(v, w) in &graph.adj[u] {
            if v <= u {
                continue; // each undirected edge once
            }
            let cv = membership[v];
            if cu == cv {
                self_mass[cu] += w;
            } else {
                let (lo, hi) = if cu < cv { (cu, cv) } else { (cv, cu) };
                *inter.entry((lo, hi)).or_insert(0.0) += w;
            }
        }
    }

    // Insert in deterministic order (HashMap iteration is process-randomised and
    // adjacency ordering feeds back into later phases).
    let mut inter_edges: Vec<((usize, usize), f64)> = inter.into_iter().collect();
    inter_edges.sort_unstable_by_key(|&((u, v), _)| (u, v));
    for ((u, v), w) in inter_edges {
        new_graph.add_edge(u, v, w);
    }
    for (node, &w) in self_mass.iter().enumerate() {
        new_graph.add_self_loop(node, w);
    }

    new_graph
}

// ── Cluster naming ────────────────────────────────────────────────────────

/// Common words excluded when extracting meaningful keywords from symbol names.
pub(crate) const STOP_WORDS: &[&str] = &[
    "get", "set", "test", "new", "is", "has", "to", "from", "with", "the", "and", "or", "of", "in",
    "at", "by", "for",
];

/// Derive a human-readable name for a cluster given its member file paths and
/// a map from file path to dominant symbol name.
///
/// Four-step heuristic (code-review-graph approach):
/// 1. Most common short directory name among members.
/// 2. If one symbol accounts for >40 % of members, use it.
/// 3. Otherwise, most frequent meaningful keyword from symbol names.
/// 4. Combine as `"{dir}-{keyword}"`, slug-cased, max 30 chars.
fn name_cluster(members: &[String], symbol_map: &HashMap<String, String>) -> String {
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
    // Pick the highest-frequency symbol; use it if it strictly exceeds the threshold.
    // Integer-safe: count * 5 > len * 2  ≡  count / len > 0.4 (DOMINANT_SYMBOL_THRESHOLD).
    if let Some((&dominant_sym, &count)) = sym_freq.iter().max_by_key(|(_, &c)| c) {
        if count * 5 > members.len() * 2 {
            return slugify(dominant_sym, SLUG_MAX_LENGTH);
        }
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
    slugify(&combined, SLUG_MAX_LENGTH)
}

/// Split a camelCase or snake_case identifier into words.
pub(crate) fn split_identifier(s: &str) -> Vec<&str> {
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
pub(crate) fn slugify(s: &str, max_len: usize) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
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

    /// Build the dependency graph, run the Leiden algorithm, and return the
    /// resulting clusters together with the raw [`crate::domain::FileGraph`].
    ///
    /// Callers that only need the [`ClusterGraph`] should use [`Self::create_clusters`].
    /// The graph is exposed here so `architecture_overview` can reuse it for
    /// inter-cluster edge aggregation without a second `build_graph` call.
    async fn create_clusters_with_graph(
        &self,
        repository_id: &str,
    ) -> Result<(ClusterGraph, crate::domain::FileGraph), DomainError> {
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
            return Ok((
                ClusterGraph {
                    clusters,
                    repository_id: repository_id.to_string(),
                    total_files: n,
                    total_edges,
                },
                graph,
            ));
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

        let cohesion_stats = batch_cohesion(&file_to_cluster, &graph.edges, &cluster_ids);

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
                let (int_e, ext_e) = cohesion_stats.get(&cid).copied().unwrap_or((0, 0));
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

        Ok((
            ClusterGraph {
                clusters,
                repository_id: repository_id.to_string(),
                total_files: n,
                total_edges,
            },
            graph,
        ))
    }

    /// Detect clusters in the dependency graph of `repository_id`.
    pub async fn create_clusters(&self, repository_id: &str) -> Result<ClusterGraph, DomainError> {
        Ok(self.create_clusters_with_graph(repository_id).await?.0)
    }

    /// Return the cluster a given file belongs to.
    pub async fn cluster_for_file(
        &self,
        file_path: &str,
        repository_id: &str,
    ) -> Result<Option<Cluster>, DomainError> {
        let (mut cg, _) = self.create_clusters_with_graph(repository_id).await?;
        // Build a file → cluster index for O(1) lookup instead of scanning all members.
        let cluster_idx: Option<usize> = cg
            .clusters
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.members.iter().any(|m| m == file_path).then_some(i));
        Ok(cluster_idx.map(|i| cg.clusters.swap_remove(i)))
    }

    /// Return a high-level architecture summary as a Markdown table.
    ///
    /// One row per cluster: name, file count, dominant language, and the top 3
    /// outgoing inter-cluster dependencies by summed edge weight.
    pub async fn architecture_overview(&self, repository_id: &str) -> Result<String, DomainError> {
        let (cg, graph) = self.create_clusters_with_graph(repository_id).await?;

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

        // Aggregate: (from_cluster_id, to_cluster_id) → total_weight
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
    fn test_enforce_connectivity_splits_disconnected_community() {
        // Two disjoint edges forced into a single community must be split into
        // two internally-connected communities.
        let mut g = Graph::new(4);
        g.add_edge(0, 1, 1.0);
        g.add_edge(2, 3, 1.0);
        let mut partition = vec![0, 0, 0, 0];
        enforce_connectivity(&g, &mut partition);
        assert_eq!(partition[0], partition[1]);
        assert_eq!(partition[2], partition[3]);
        assert_ne!(partition[0], partition[2]);
    }

    #[test]
    fn test_enforce_connectivity_keeps_connected_intact() {
        // A genuinely connected community is left untouched (one label).
        let mut g = Graph::new(3);
        g.add_edge(0, 1, 1.0);
        g.add_edge(1, 2, 1.0);
        let mut partition = vec![7, 7, 7];
        enforce_connectivity(&g, &mut partition);
        assert_eq!(partition[0], partition[1]);
        assert_eq!(partition[1], partition[2]);
    }

    #[test]
    fn test_split_oversized_subdivides_dominant_community() {
        // Two 5-cliques joined by one weak bridge, forced into a single
        // community (size 10 = 100% of the graph). split_oversized must break it
        // back into the two cliques.
        let mut g = Graph::new(10);
        for i in 0..5 {
            for j in (i + 1)..5 {
                g.add_edge(i, j, 1.0);
            }
        }
        for i in 5..10 {
            for j in (i + 1)..10 {
                g.add_edge(i, j, 1.0);
            }
        }
        g.add_edge(0, 5, 0.1); // weak bridge

        let mut partition = vec![0; 10];
        split_oversized(&g, &mut partition);

        assert!(
            partition[0..5].iter().all(|&l| l == partition[0]),
            "first clique should share one label: {:?}",
            partition
        );
        assert!(
            partition[5..10].iter().all(|&l| l == partition[5]),
            "second clique should share one label: {:?}",
            partition
        );
        assert_ne!(
            partition[0], partition[5],
            "the two cliques should land in different communities: {:?}",
            partition
        );
    }

    #[test]
    fn test_split_oversized_leaves_small_communities() {
        // Below MIN_SPLIT_SIZE nothing is touched even if one label dominates.
        let mut g = Graph::new(4);
        g.add_edge(0, 1, 1.0);
        g.add_edge(1, 2, 1.0);
        g.add_edge(2, 3, 1.0);
        let mut partition = vec![0, 0, 0, 0];
        split_oversized(&g, &mut partition);
        assert!(partition.iter().all(|&l| l == 0));
    }

    /// Build two `size`-cliques joined by a single weak bridge edge.
    fn two_cliques(size: usize, bridge_weight: f64) -> Graph {
        let mut g = Graph::new(size * 2);
        for (base, _) in [(0usize, ()), (size, ())] {
            for i in base..base + size {
                for j in (i + 1)..base + size {
                    g.add_edge(i, j, 1.0);
                }
            }
        }
        g.add_edge(0, size, bridge_weight);
        g
    }

    #[test]
    fn test_leiden_separates_two_cliques() {
        // The real refinement must recover the two cliques as separate,
        // internally-connected communities.
        let g = two_cliques(6, 0.05);
        let partition = leiden(&g);
        assert!(
            partition[0..6].iter().all(|&l| l == partition[0]),
            "first clique split: {:?}",
            partition
        );
        assert!(
            partition[6..12].iter().all(|&l| l == partition[6]),
            "second clique split: {:?}",
            partition
        );
        assert_ne!(
            partition[0], partition[6],
            "cliques merged: {:?}",
            partition
        );
    }

    #[test]
    fn test_leiden_is_deterministic() {
        // Seeded refinement ⇒ identical partitions across repeated runs.
        let g = two_cliques(8, 0.1);
        assert_eq!(leiden(&g), leiden(&g));
    }

    #[test]
    fn test_leiden_communities_are_connected() {
        // Every community Leiden returns must be a single connected component of
        // the induced subgraph (the guarantee that distinguishes it from Louvain).
        let g = two_cliques(7, 0.05);
        let partition = leiden(&g);

        let num = partition.iter().copied().max().unwrap() + 1;
        for community in 0..num {
            let members: Vec<usize> = (0..g.n).filter(|&i| partition[i] == community).collect();
            if members.len() < 2 {
                continue;
            }
            // BFS within the community from the first member.
            let set: std::collections::HashSet<usize> = members.iter().copied().collect();
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![members[0]];
            seen.insert(members[0]);
            while let Some(u) = stack.pop() {
                for &(v, _) in &g.adj[u] {
                    if set.contains(&v) && seen.insert(v) {
                        stack.push(v);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                members.len(),
                "community {} is not internally connected: {:?}",
                community,
                members
            );
        }
    }

    #[test]
    fn test_name_cluster_uses_dir() {
        let members = vec![
            "src/auth/login.rs".to_string(),
            "src/auth/logout.rs".to_string(),
        ];
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
