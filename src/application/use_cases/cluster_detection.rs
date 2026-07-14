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

// ── Weighted-degree helpers (used by the façade split's god-object gate) ───

/// Weighted degree per node from an undirected edge list over `n` nodes.
fn weighted_degrees(n: usize, edges: &[(usize, usize, f64)]) -> Vec<f64> {
    let mut deg = vec![0.0f64; n];
    for &(u, v, w) in edges {
        deg[u] += w;
        deg[v] += w;
    }
    deg
}

/// The weighted-degree threshold at the given percentile (nearest-rank on the
/// sorted non-zero degrees). Returns `f64::INFINITY` when there is nothing to
/// threshold, so no node is ever flagged.
fn degree_threshold_at(degrees: &[f64], percentile: f64) -> f64 {
    let mut sorted: Vec<f64> = degrees.iter().copied().filter(|d| *d > 0.0).collect();
    if sorted.is_empty() {
        return f64::INFINITY;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Nearest-rank: rank = ceil(p/100 * N), clamped into [1, N].
    let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted.len()) - 1;
    sorted[idx]
}

// ── Coupling-informed façade split (experimental) ─────────────────────────
//
// A god-object (a shared constants class, a base exception, a utility
// grab-bag) is a single node wired to hundreds of otherwise-unrelated nodes.
// The hub pre-filter above blunts it by scaling or dropping its edges, but that
// is a blunt instrument: down-weighting keeps the node as the *only* bridge
// between two blocks (so it stays a coupler), and dropping shatters the graph.
//
// The façade split is surgical. It first asks the coupling pipeline which nodes
// are *verified* couplers, then replaces each such god-object `H` with one
// **façade per neighbouring community**: every edge `H—v` is re-attached to the
// façade `H@community(v)`. No edge weight is lost, but `H` is no longer a single
// vertex two communities can route a path through — the false glue is gone
// while every real dependency survives. After Leiden runs on the façaded graph,
// the façades are collapsed back to `H`, which is assigned to whichever
// community its façades carry the most weight into.
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

/// Partition `n` nodes (named by `names`, connected by `edges`) using the
/// coupling-informed façade split, returning a label per original node.
///
/// Steps: (1) build the raw graph, (2) [`super::coupling_detection::detect_god_objects`]
/// selects verified couplers above the degree gate, (3) explode each into
/// per-neighbour-community façades, (4) run [`leiden`] on the expanded graph,
/// (5) collapse façades back so every original node gets exactly one label.
///
/// Deterministic: the god-object list, façade order, and edge insertion are all
/// sorted, and the collapse tie-breaks on the smallest label.
pub(crate) fn partition_with_facade_split(
    names: &[String],
    edges: &[(usize, usize, f64)],
    degree_percentile: f64,
) -> Vec<usize> {
    let n = names.len();
    let mut raw = Graph::new(n);
    for &(u, v, w) in edges {
        raw.add_edge(u, v, w);
    }

    // Degree gate: the percentile over the raw weighted-degree distribution.
    let degrees = weighted_degrees(n, edges);
    let min_degree = degree_threshold_at(&degrees, degree_percentile);

    let gods = super::coupling_detection::detect_god_objects(&raw, names, min_degree);
    if gods.is_empty() {
        // Nothing to split — behave exactly like plain detection.
        let mut p = leiden(&raw);
        renumber(&mut p);
        return p;
    }
    let god_set: HashSet<usize> = gods.iter().map(|g| g.node).collect();

    // Baseline partition drives which community each neighbour belongs to.
    let baseline = leiden(&raw);

    // Build the façaded graph over a *compact* index space that excludes the
    // god originals entirely: a split god-object has all its edges re-routed to
    // façades, so keeping its original index would leave an isolated singleton
    // that inflates `expanded.n` and the tiny-community fraction
    // `select_resolution` reads — biasing the resolution search on many-god
    // graphs. `origin[i]` maps every expanded node back to an original node so
    // the partition can be collapsed afterwards: a non-god original keeps its
    // identity; a façade points at its god.
    let mut origin: Vec<usize> = Vec::with_capacity(n);
    // Original (non-god) node index → its compact index in the expanded graph.
    let mut compact_of: Vec<Option<usize>> = vec![None; n];
    for (u, slot) in compact_of.iter_mut().enumerate() {
        if !god_set.contains(&u) {
            *slot = Some(origin.len());
            origin.push(u);
        }
    }
    // (god node, neighbour-community) → façade node index.
    let mut facade_of: HashMap<(usize, usize), usize> = HashMap::new();

    // Resolve the façade index for god `g`'s edge toward a neighbour in
    // community `comm`, creating it on first use.
    let facade_index = |g: usize,
                        comm: usize,
                        origin: &mut Vec<usize>,
                        facade_of: &mut HashMap<(usize, usize), usize>|
     -> usize {
        *facade_of.entry((g, comm)).or_insert_with(|| {
            let idx = origin.len();
            origin.push(g);
            idx
        })
    };

    // Map one edge endpoint to its expanded index: a non-god node has a compact
    // index in `compact_of`; a god node (compact index `None`) is redirected to
    // its façade for the *other* endpoint's community. `compact_of` thus doubles
    // as the god test, so no separate membership lookup or unwrap is needed.
    let endpoint_index = |node: usize,
                          other_community: usize,
                          origin: &mut Vec<usize>,
                          facade_of: &mut HashMap<(usize, usize), usize>|
     -> usize {
        match compact_of[node] {
            Some(idx) => idx,
            None => facade_index(node, other_community, origin, facade_of),
        }
    };

    // Deterministic edge list of the façaded graph, in compact indices.
    let mut new_edges: Vec<(usize, usize, f64)> = Vec::with_capacity(edges.len());
    for &(u, v, w) in edges {
        let su = endpoint_index(u, baseline[v], &mut origin, &mut facade_of);
        let sv = endpoint_index(v, baseline[u], &mut origin, &mut facade_of);
        if su == sv {
            continue; // god↔god edge within the same façade bucket: skip self-loop
        }
        new_edges.push((su, sv, w));
    }

    let expanded_n = origin.len();
    let mut expanded = Graph::new(expanded_n);
    // Deduplicate + deterministic order (façade routing can produce parallels).
    let mut merged: HashMap<(usize, usize), f64> = HashMap::new();
    for (u, v, w) in new_edges {
        let (lo, hi) = if u < v { (u, v) } else { (v, u) };
        *merged.entry((lo, hi)).or_insert(0.0) += w;
    }
    let mut merged_edges: Vec<((usize, usize), f64)> = merged.into_iter().collect();
    merged_edges.sort_unstable_by_key(|&((u, v), _)| (u, v));
    for ((u, v), w) in &merged_edges {
        expanded.add_edge(*u, *v, *w);
    }

    let expanded_partition = leiden(&expanded);

    // Collapse the expanded partition back to one label per original node.
    // A non-god original reads its compact node's label directly; a god-object
    // lands in the community its façades carry the most edge weight into (ties
    // break toward the smaller label). Weight is summed over the façades only,
    // keyed by the *original* god node via `origin`.
    let mut weight_into: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
    for &((u, v), w) in &merged_edges {
        let (ou, ov) = (origin[u], origin[v]);
        if god_set.contains(&ou) {
            *weight_into[ou].entry(expanded_partition[u]).or_insert(0.0) += w;
        }
        if god_set.contains(&ov) {
            *weight_into[ov].entry(expanded_partition[v]).or_insert(0.0) += w;
        }
    }

    // A community label that no façade or non-god node occupies — the fallback
    // home for a god-object whose every edge was a dropped god↔god self-loop.
    // Giving it a fresh isolated label keeps it out of an arbitrary community.
    let mut next_label = expanded_partition
        .iter()
        .copied()
        .max()
        .map_or(0, |m| m + 1);

    let mut labels: Vec<usize> = vec![usize::MAX; n];
    for (orig, slot) in labels.iter_mut().enumerate() {
        *slot = match compact_of[orig] {
            // Non-god node: inherit its compact node's label.
            Some(idx) => expanded_partition[idx],
            // God node: heaviest façade community, else a fresh isolated label.
            None => weight_into[orig]
                .iter()
                .max_by(|a, b| {
                    a.1.partial_cmp(b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(b.0.cmp(a.0))
                })
                .map(|(&label, _)| label)
                .unwrap_or_else(|| {
                    let l = next_label;
                    next_label += 1;
                    l
                }),
        };
    }
    renumber(&mut labels);
    if let Some(top) = gods.first() {
        debug!(
            "facade split: {} god-objects exploded into {} façades ({} originals → {} expanded nodes); \
             strongest: {} (degree {:.1}, coupling strength {:.2})",
            gods.len(),
            expanded_n - (n - gods.len()),
            n,
            expanded_n,
            names[top.node],
            top.degree,
            top.coupling_strength,
        );
    }
    labels
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

    /// Number of nodes.
    pub(crate) fn node_count(&self) -> usize {
        self.n
    }

    /// Adjacency of `u`: `(neighbour, weight)` pairs (undirected, so every edge
    /// appears in both endpoints' lists).
    pub(crate) fn neighbors(&self, u: usize) -> &[(usize, f64)] {
        &self.adj[u]
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

    /// The deduplicated undirected edge list `(lo, hi, weight)`, sorted for
    /// determinism. Each undirected edge appears once (`lo < hi`); self-loops
    /// are excluded. Used by the façade split, which needs to rebuild the graph
    /// with god-object nodes exploded.
    pub(crate) fn edge_list(&self) -> Vec<(usize, usize, f64)> {
        let mut edges: Vec<(usize, usize, f64)> = Vec::new();
        for u in 0..self.n {
            for &(v, w) in &self.adj[u] {
                if u < v {
                    edges.push((u, v, w));
                }
            }
        }
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges
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

/// Target ceiling on the largest community's share of the graph used by the
/// dynamic resolution search ([`select_resolution`]). Resolution is increased
/// until the biggest community fits under this fraction, which is what breaks
/// the modularity resolution-limit mega-blobs (a single "community" holding
/// 10–25 % of every symbol in the repo, mixing unrelated subsystems) into
/// coherent units. A large service has dozens of real modules, so no single one
/// should own more than a small slice; 6 % keeps the biggest honest without
/// forcing over-fragmentation (the `< 0.95` progress guard in
/// [`select_resolution`] stops climbing once splitting stops helping).
const TARGET_MAX_FRACTION: f64 = 0.06;
/// Candidate resolutions swept by [`select_resolution`], ascending. `1.0` is
/// classic modularity; higher values pull the null-model penalty up, favouring
/// more, smaller communities. Capped so the search cannot shatter a genuinely
/// cohesive graph into dust.
const RESOLUTION_LADDER: &[f64] = &[1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0];

/// Minimum node count before the dynamic resolution search runs. The modularity
/// resolution limit only produces mega-blobs on large graphs; on small graphs
/// classic modularity (γ = 1) already recovers the right structure, and raising
/// the resolution there would over-split genuinely tight groups (e.g. splitting
/// a connected pair). Below this size we keep γ = 1.
const MIN_NODES_FOR_RESOLUTION_SEARCH: usize = 60;

/// If a resolution puts more than this fraction of nodes into tiny (≤ 2 node)
/// communities, it has shattered the graph rather than de-blobbed it, and the
/// resolution search backs off to the last coherent resolution
/// (see [`select_resolution`]).
const MAX_FRAGMENT_RATIO: f64 = 0.5;

/// Run Leiden cluster detection on `graph` at automatically-selected resolution
/// and return a partition: a `Vec<usize>` where `partition[node_index]` is the
/// cluster id.
///
/// The resolution is chosen by [`select_resolution`]; the partition is then the
/// Leiden core ([`leiden_core`]) followed by the two guarantee-enforcing
/// post-passes ([`split_oversized`] then [`enforce_connectivity`]), with labels
/// renumbered contiguously at the end.
pub(crate) fn leiden(graph: &Graph) -> Vec<usize> {
    if graph.n == 0 {
        return Vec::new();
    }
    // `select_resolution` already ran `leiden_core` at the chosen γ while
    // searching; reuse that partition instead of recomputing it.
    let (gamma, mut result) = select_resolution(graph);

    // Split any community that dominates the graph (uses the bare core on the
    // induced subgraph, so this does not recurse through the post-passes).
    split_oversized(graph, &mut result, gamma);
    // Belt-and-braces: true Leiden refinement already yields connected
    // communities, but the oversized split and any future change could not, so
    // re-assert the guarantee as the final step.
    enforce_connectivity(graph, &mut result);
    renumber(&mut result);
    result
}

/// Choose a resolution for `graph` dynamically.
///
/// Modularity has a well-known resolution limit: on a large, densely
/// interconnected graph its optimum merges many small, genuinely distinct
/// modules into a handful of giant blobs (here, symbol communities holding
/// 15–25 % of the whole repo that mix unrelated subsystems). Rather than hard-
/// code one resolution, we sweep [`RESOLUTION_LADDER`] and pick the *smallest*
/// (coarsest) resolution whose largest community fits under
/// [`TARGET_MAX_FRACTION`]. Coarsest-that-fits keeps communities as large as
/// they can be without a blob dominating, so we neither under- nor over-split.
///
/// If no resolution on the ladder gets under the target (an unusually monolithic
/// graph, or one whose natural modules are larger than the target), the search
/// stops before it starts *shattering* the graph: once raising γ produces mostly
/// tiny fragments (a fragmentation ratio past [`MAX_FRAGMENT_RATIO`]) rather than
/// splitting blobs into real modules, it falls back to the last resolution that
/// still yielded coherent communities. Graphs below
/// [`MIN_NODES_FOR_RESOLUTION_SEARCH`] keep γ = 1.
/// Returns the chosen resolution *and* the `leiden_core` partition computed at
/// it, so the caller need not re-run the core a final time.
fn select_resolution(graph: &Graph) -> (f64, Vec<usize>) {
    if graph.n < MIN_NODES_FOR_RESOLUTION_SEARCH {
        return (1.0, leiden_core(graph, 1.0));
    }
    let target = (graph.n as f64 * TARGET_MAX_FRACTION).ceil() as usize;

    // Among non-shattering resolutions, remember the one whose largest community
    // is smallest — the best de-blobbing that still leaves real modules intact.
    // Seeded from the first (coarsest) rung so a `best` partition always exists,
    // even if every rung shatters or overshoots the target.
    let mut best: Option<(f64, Vec<usize>)> = None;
    let mut best_largest = usize::MAX;
    for &gamma in RESOLUTION_LADDER {
        let partition = leiden_core(graph, gamma);
        let (largest, tiny_fraction) = partition_shape(&partition);

        // Stop as soon as a resolution shatters the graph into mostly-tiny
        // fragments: higher γ from here only makes it worse, and the previously
        // recorded best is the coherent choice.
        if tiny_fraction > MAX_FRAGMENT_RATIO {
            // `best` is set unless the very first rung shatters; in that pathological
            // case fall back to this rung's partition rather than none.
            let chosen = best.unwrap_or((gamma, partition));
            debug!(
                "select_resolution: gamma={gamma} shatters (fragments={tiny_fraction:.2}); \
                 stopping at best_gamma={} (best_largest={best_largest}, n={})",
                chosen.0, graph.n
            );
            return chosen;
        }

        if largest < best_largest {
            best = Some((gamma, partition.clone()));
            best_largest = largest;
        }

        if largest <= target {
            debug!(
                "select_resolution: gamma={gamma} largest={largest} target={target} (n={})",
                graph.n
            );
            return (gamma, partition);
        }
    }
    let chosen = best.expect("RESOLUTION_LADDER is non-empty so best is always set");
    debug!(
        "select_resolution: no gamma met target={target}; using best_gamma={} \
         (best_largest={best_largest}, n={})",
        chosen.0, graph.n
    );
    chosen
}

/// The largest community's size and the fraction of nodes in "tiny" (≤ 2 node)
/// communities, computed in one pass. A high tiny-fraction means the resolution
/// has shattered the graph into singletons/pairs rather than into real modules.
fn partition_shape(partition: &[usize]) -> (usize, f64) {
    if partition.is_empty() {
        return (0, 0.0);
    }
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &label in partition {
        *counts.entry(label).or_insert(0) += 1;
    }
    let largest = counts.values().copied().max().unwrap_or(0);
    let tiny: usize = counts.values().filter(|&&c| c <= 2).sum();
    (largest, tiny as f64 / partition.len() as f64)
}

/// Size of the largest community in a partition. Used only by tests now that
/// [`select_resolution`] gets both metrics it needs from [`partition_shape`].
#[cfg(test)]
fn largest_community_size(partition: &[usize]) -> usize {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let mut max = 0;
    for &label in partition {
        let c = counts.entry(label).or_insert(0);
        *c += 1;
        max = max.max(*c);
    }
    max
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
///
/// `gamma` is the resolution: it scales the null-model (expected-edge) term in
/// every gain and in [`modularity`], so `gamma > 1` favours more, smaller
/// communities. `gamma == 1` is classic modularity.
fn leiden_core(graph: &Graph, gamma: f64) -> Vec<usize> {
    leiden_core_seeded(graph, gamma, LEIDEN_SEED)
}

/// [`leiden_core`] with an explicit refinement seed.
///
/// The default entry points pin the seed to [`LEIDEN_SEED`] so partitions are
/// reproducible; coupling detection ([`super::coupling_detection`]) instead
/// *needs* the seed-to-seed variation — it re-clusters a community's subgraph
/// under many seeds to estimate how probable a split is, rather than trusting a
/// single stochastic outcome. Same algorithm, same guarantees, different seed.
pub(crate) fn leiden_core_seeded(graph: &Graph, gamma: f64, seed: u64) -> Vec<usize> {
    if graph.n == 0 {
        return Vec::new();
    }

    let mut rng = SplitMix64::new(seed);
    let mut current = graph.clone();
    // Partition `p` over the current (aggregated) graph's nodes.
    let mut partition: Vec<usize> = (0..current.n).collect();
    // Map every original node to its node index in `current`.
    let mut node_to_super: Vec<usize> = (0..graph.n).collect();
    let mut prev_modularity = f64::NEG_INFINITY;

    for _ in 0..MAX_ITERATIONS {
        local_moving_phase(&current, &mut partition, gamma);
        renumber(&mut partition);

        let num_communities = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let q = modularity(&current, &partition, gamma);

        // Nothing left to aggregate (every node already its own community) or no
        // meaningful modularity improvement: stop with the current partition.
        if num_communities >= current.n || q - prev_modularity < MIN_MODULARITY_GAIN {
            break;
        }
        prev_modularity = q;

        // Refine each community into well-connected sub-communities.
        let mut refined = refine_partition(&current, &partition, &mut rng, gamma);
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
pub(crate) fn group_by_label(partition: &[usize]) -> BTreeMap<usize, Vec<usize>> {
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
/// on its induced subgraph at resolution `gamma`. The first resulting
/// sub-community keeps the original label; the rest receive fresh labels.
/// Single-level only: if the subgraph is indivisible the community is left
/// as-is.
fn split_oversized(graph: &Graph, partition: &mut [usize], gamma: f64) {
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

        let mut sub_partition = leiden_core(&sub, gamma);
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

/// Modularity Q = (1/2m) Σ_ij [ A_ij - γ·k_i k_j / 2m ] δ(c_i, c_j)
///
/// `gamma` (γ) is the resolution: γ > 1 inflates the expected-edge penalty, so
/// keeping two nodes together must overcome a larger null-model term, which
/// yields more, smaller communities.
fn modularity(graph: &Graph, partition: &[usize], gamma: f64) -> f64 {
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
    let penalty: f64 = gamma * cluster_degree.values().map(|&d| d * d).sum::<f64>() / (m2 * m2);
    q - penalty
}

/// Local moving phase: repeatedly scan all nodes and move each to the
/// neighbouring cluster that maximises the modularity gain at resolution
/// `gamma` (which scales the null-model term of every gain).
fn local_moving_phase(graph: &Graph, partition: &mut Vec<usize>, gamma: f64) {
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

            // Modularity gain of removing u from cu (null-model term scaled by γ).
            let sigma_cu = *cluster_total.get(&cu).unwrap_or(&0.0);
            let remove_gain = ku_in - gamma * ku * (sigma_cu - ku) / m2;

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
                let gain = w_to_ct - gamma * ku * sigma_ct / m2 + remove_gain;
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
///
/// `gamma` scales the null-model term of the merge gain, matching the resolution
/// used in the local-moving phase so refinement splits at the same granularity.
fn refine_partition(
    graph: &Graph,
    community: &[usize],
    rng: &mut SplitMix64,
    gamma: f64,
) -> Vec<usize> {
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
            // Merging a singleton into c: gain = w_to_c - γ · k_v · Σ_c / 2m
            // (the singleton has no internal mass, so its removal cost is 0).
            let gain = w_to_c - gamma * kv * sub_degree[c] / m2;
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
pub(crate) fn renumber(partition: &mut Vec<usize>) {
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
            None => leiden(&g),
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

        for cluster in &cg.clusters {
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
        split_oversized(&g, &mut partition, 1.0);

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
        split_oversized(&g, &mut partition, 1.0);
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
    fn test_renumber() {
        let mut p = vec![5, 5, 10, 10, 5];
        renumber(&mut p);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[4]);
        assert_ne!(p[0], p[2]);
        assert_eq!(p[2], p[3]);
    }

    #[test]
    fn test_select_resolution_breaks_up_blob() {
        // Ten tightly-connected 8-cliques, chained by weak bridges into one big
        // graph (80 nodes). Classic modularity (γ=1) merges neighbouring cliques
        // into blobs; the dynamic resolution search must raise γ until no single
        // community dominates, recovering finer structure.
        const CLIQUES: usize = 10;
        const SIZE: usize = 8;
        let mut g = Graph::new(CLIQUES * SIZE);
        for c in 0..CLIQUES {
            let base = c * SIZE;
            for i in base..base + SIZE {
                for j in (i + 1)..base + SIZE {
                    g.add_edge(i, j, 1.0);
                }
            }
            if c > 0 {
                g.add_edge(base, base - SIZE, 0.05); // weak bridge to previous clique
            }
        }

        let partition = leiden(&g);
        let largest = largest_community_size(&partition);
        let target = (g.n as f64 * TARGET_MAX_FRACTION).ceil() as usize;
        assert!(
            largest <= target.max(SIZE),
            "largest community {largest} should fit the resolution target {target} \
             (n={})",
            g.n
        );
        // Sanity: we did not shatter the cliques into dust.
        let num = partition.iter().copied().max().unwrap() + 1;
        assert!(
            (CLIQUES..=CLIQUES * 2).contains(&num),
            "expected ~{CLIQUES} communities, got {num}"
        );
    }

    // ── Façade split ──────────────────────────────────────────────────────

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
        let mut plain = leiden(&g);
        renumber(&mut plain);
        let facade = partition_with_facade_split(&names, &g.edge_list(), 99.0);
        assert_eq!(plain, facade);
    }
}
