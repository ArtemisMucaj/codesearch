//! Coupling-element detection: find the glue holding fragile Leiden
//! communities together.
//!
//! A **coupling element** is defined counterfactually: node or edge X couples
//! community C when C is a single community in the baseline Leiden partition,
//! but removing X makes C's nodes split into two blocks A and B. The
//! load-bearing case is a community that is *internally* two latent sub-blocks
//! held together by one file, symbol, or dependency — the classic hub-like
//! dependency / modularity-violation smell.
//!
//! Ablating every element and re-clustering globally would cost
//! `O(elements × leiden(E))`; instead this runs the well-known
//! **filter-then-verify** pipeline, all of it local to one community at a
//! time:
//!
//! 1. **Localize** — re-cluster each community's induced subgraph across a
//!    resolution ladder ([`GAMMA_LADDER`]). A community that never separates
//!    has no internal 2-block structure and is skipped outright. One that
//!    holds at γ ≤ [`CommunityCoupling::gamma_hold`] and separates at
//!    [`CommunityCoupling::gamma_split`] is *fragile*: the partition at the
//!    split resolution names the latent sub-blocks {A, B}.
//! 2. **Score candidates cheaply** — the minimum cut between A and B is
//!    literally the glue edge set (edge couplers); aggregating cut shares onto
//!    incident nodes plus the Guimerà–Amaral participation coefficient ranks
//!    node couplers. Both are `O(E_C)`-ish and involve no re-clustering.
//! 3. **Verify by ablation** — remove each surviving candidate from the
//!    subgraph and re-cluster at `gamma_hold` under [`VERIFY_RUNS`] different
//!    refinement seeds. Leiden is stochastic, so a single run is noise; the
//!    *fraction* of runs where A separates from B — compared against the same
//!    fraction with the element still present — is the verdict.
//! 4. **Sweep resolution** — repeat the verification at every ladder rung
//!    below `gamma_hold` to report the γ range over which the element
//!    controls the merge, rather than a yes/no at one arbitrary resolution.
//!
//! The algorithm and graph primitives are reused verbatim from
//! [`super::cluster_detection`] so the baseline partition here is identical to
//! what the `clusters` / `symbol-clusters` commands report (including the
//! stable community ids).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use super::cluster_detection::{
    build_file_leiden_graph, group_by_label, leiden, leiden_core_seeded, qualify_namespace_graph,
    renumber, Graph,
};
use super::{FileRelationshipUseCase, SymbolClusterDetectionUseCase};
use crate::domain::{
    namespace_scope_id, stable_community_id, CommunityCoupling, CouplingElement,
    CouplingElementKind, CouplingReport, DomainError, GraphLevel,
};

// ── Tuning constants ──────────────────────────────────────────────────────

/// Communities smaller than this cannot contain two meaningful sub-blocks
/// (each of at least [`MIN_BLOCK_SIZE`] nodes) and are skipped.
const MIN_COMMUNITY_SIZE: usize = 4;

/// A separation only counts as revealing 2-block structure when the two
/// largest blocks each have at least this many nodes — a single leaf falling
/// off is not a latent module boundary.
const MIN_BLOCK_SIZE: usize = 2;

/// Resolutions probed on each community's induced subgraph, ascending. The
/// ladder starts far below classic modularity (γ = 1) because a community that
/// the *global* partition kept whole often separates locally at small γ: the
/// subgraph's total weight is tiny, so the null-model penalty that hid the
/// fault line globally (the resolution limit) no longer does. At the bottom
/// rung any connected subgraph holds together, so `gamma_hold` exists for
/// every community with genuine (if weak) glue.
const GAMMA_LADDER: &[f64] = &[0.05, 0.1, 0.2, 0.4, 0.7, 1.0, 1.5, 2.0, 3.0];

/// Seeded re-clusterings per probability estimate. Split probabilities are
/// therefore multiples of 1/8 — coarse, but plenty to separate "falls apart
/// without X" from "was going to fall apart anyway".
const VERIFY_RUNS: usize = 8;

/// Refinement seed for the deterministic fragility probe (step 1).
const PROBE_SEED: u64 = 0x5EED_C0DE_CAFE_F00D;

/// Base seed for the verification runs; run `i` uses a SplitMix64-style
/// derivation so the runs are decorrelated but reproducible.
const VERIFY_SEED_BASE: u64 = 0xB10C_5EED_0DDC_0DE5;

/// Top node / edge candidates promoted from the cheap scoring to ablation.
const MAX_NODE_CANDIDATES: usize = 5;
const MAX_EDGE_CANDIDATES: usize = 5;

/// Minimum `split_probability − baseline_split_probability` for a candidate
/// to be reported as a verified coupler. Below this, removing the element
/// barely changes the outcome — it is not the glue.
const MIN_COUPLING_STRENGTH: f64 = 0.25;

/// Split probability at or above which a γ rung counts as *actively*
/// controlled by the coupler (used for the reported γ range).
const ACTIVE_SPLIT_PROBABILITY: f64 = 0.5;

/// Weights below this are treated as zero in the max-flow residual graph.
const FLOW_EPS: f64 = 1e-12;

fn verify_seed(run: usize) -> u64 {
    VERIFY_SEED_BASE.wrapping_add((run as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

// ── Use case ──────────────────────────────────────────────────────────────

/// Use case: detect coupling elements in a repository's Leiden communities, at
/// either the file or the symbol level.
pub struct CouplingDetectionUseCase {
    file_graph: Arc<FileRelationshipUseCase>,
    symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
}

impl CouplingDetectionUseCase {
    pub fn new(
        file_graph: Arc<FileRelationshipUseCase>,
        symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
    ) -> Self {
        Self {
            file_graph,
            symbol_clusters,
        }
    }

    /// Run the full filter-then-verify pipeline on `repository_id` at `level`.
    ///
    /// The graph and baseline partition are rebuilt with the same code paths
    /// as cluster / symbol-community detection, so community ids in the report
    /// match those commands' output for unchanged memberships.
    pub async fn detect(
        &self,
        repository_id: &str,
        level: GraphLevel,
    ) -> Result<CouplingReport, DomainError> {
        let (names, graph, id_prefix) = match level {
            GraphLevel::File => {
                let fg = self
                    .file_graph
                    .build_graph(Some(&[repository_id.to_string()]), 1, false)
                    .await?;
                let (files, g) = build_file_leiden_graph(&fg);
                (files, g, "c")
            }
            GraphLevel::Symbol => {
                let sg = self
                    .symbol_clusters
                    .build_symbol_graph(repository_id)
                    .await?;
                (sg.symbols, sg.graph, "s")
            }
        };

        let analysis = analyze_graph(&graph, &names, id_prefix);
        Ok(CouplingReport {
            repository_id: repository_id.to_string(),
            level,
            total_communities: analysis.total_communities,
            fragile_communities: analysis.fragile_communities,
            communities: analysis.communities,
        })
    }

    /// Run the coupling pipeline over the **namespace-wide** graph — every
    /// repository in `namespace`, cross-repository edges included — at `level`.
    ///
    /// This is where global couplings earn their keep: a coupler that splits a
    /// namespace-wide community is the shared file/symbol welding two
    /// repositories together (a leaky service boundary). File-level couplers are
    /// reported as `repo:path` (the qualified node labels), so it's clear which
    /// repository's element is the glue; symbol-level couplers are FQNs, which
    /// are already globally unique.
    ///
    /// The report is keyed under the per-namespace sentinel scope id, matching
    /// the namespace cluster / symbol-community runs.
    pub async fn detect_namespace(
        &self,
        namespace: &str,
        level: GraphLevel,
    ) -> Result<CouplingReport, DomainError> {
        let (names, graph, id_prefix) = match level {
            GraphLevel::File => {
                // Every repo, cross-repo edges included, nodes qualified `repo:path`
                // exactly as the namespace file-cluster / graph-view paths do.
                let fg = qualify_namespace_graph(self.file_graph.build_graph(None, 1, true).await?);
                let (files, g) = build_file_leiden_graph(&fg);
                (files, g, "c")
            }
            GraphLevel::Symbol => {
                let sg = self
                    .symbol_clusters
                    .build_namespace_symbol_graph(Some(namespace))
                    .await?;
                (sg.symbols, sg.graph, "s")
            }
        };

        let analysis = analyze_graph(&graph, &names, id_prefix);
        Ok(CouplingReport {
            repository_id: namespace_scope_id(namespace),
            level,
            total_communities: analysis.total_communities,
            fragile_communities: analysis.fragile_communities,
            communities: analysis.communities,
        })
    }
}

// ── Whole-graph analysis ──────────────────────────────────────────────────

struct GraphAnalysis {
    total_communities: usize,
    fragile_communities: usize,
    communities: Vec<CommunityCoupling>,
}

/// Baseline-partition the graph, then run the per-community pipeline on every
/// community large enough to hold 2-block structure.
fn analyze_graph(graph: &Graph, names: &[String], id_prefix: &str) -> GraphAnalysis {
    debug_assert_eq!(graph.node_count(), names.len());
    let partition = leiden(graph);
    let by_label = group_by_label(&partition);
    let total_communities = by_label.len();

    let mut fragile_communities = 0;
    let mut communities: Vec<CommunityCoupling> = Vec::new();

    for (_label, members) in by_label {
        if members.len() < MIN_COMMUNITY_SIZE {
            continue;
        }
        let sub = CommunitySubgraph::extract(graph, &members);
        let Some(fragility) = probe_fragility(&sub) else {
            continue;
        };
        fragile_communities += 1;

        let couplers = verify_couplers(&sub, &fragility);
        if couplers.is_empty() {
            continue;
        }

        let mut member_names: Vec<String> = members.iter().map(|&g| names[g].clone()).collect();
        member_names.sort();
        let name_of = |locals: &[usize]| -> Vec<String> {
            let mut v: Vec<String> = locals
                .iter()
                .map(|&l| names[sub.globals[l]].clone())
                .collect();
            v.sort();
            v
        };

        communities.push(CommunityCoupling {
            community_id: stable_community_id(id_prefix, &member_names),
            size: members.len(),
            gamma_hold: fragility.gamma_hold,
            gamma_split: fragility.gamma_split,
            sub_block_a: name_of(&fragility.block_a),
            sub_block_b: name_of(&fragility.block_b),
            couplers: couplers
                .into_iter()
                .map(|c| c.into_element(&sub, names))
                .collect(),
        });
    }

    // Strongest coupler first; size then id break ties deterministically.
    communities.sort_by(|a, b| {
        let sa = a
            .couplers
            .first()
            .map(|c| c.coupling_strength)
            .unwrap_or(0.0);
        let sb = b
            .couplers
            .first()
            .map(|c| c.coupling_strength)
            .unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.size.cmp(&a.size))
            .then(a.community_id.cmp(&b.community_id))
    });

    GraphAnalysis {
        total_communities,
        fragile_communities,
        communities,
    }
}

// ── God-object detection (drives the façade split) ────────────────────────

/// A verified node coupler proposed for façade-splitting, resolved to its graph
/// node index with the strength that earned it a place.
pub(crate) struct GodObject {
    /// Graph node index of the coupling node.
    pub node: usize,
    /// `split_probability − baseline` from the ablation verification — how
    /// decisively removing this node splits the community it couples.
    pub coupling_strength: f64,
    /// Weighted degree of the node in the full graph — the "god-object" gate: a
    /// coupler is only worth splitting into façades if it is globally hub-like.
    pub degree: f64,
}

/// Identify the god-object coupling nodes in `graph`: run the full coupling
/// pipeline, take every *verified node coupler*, and keep the ones whose global
/// weighted degree clears `min_degree`. Returned in a deterministic order
/// (descending degree, then ascending node index).
///
/// This is the selection stage of the coupling-informed façade split: unlike a
/// raw degree-percentile filter, a node qualifies only if the ablation
/// verification already proved it holds a community together — degree is just
/// the gate that separates a true god-object (glues many communities by
/// ubiquity) from a small local hub (legitimately central to one module).
pub(crate) fn detect_god_objects(
    graph: &Graph,
    names: &[String],
    min_degree: f64,
) -> Vec<GodObject> {
    let analysis = analyze_graph(graph, names, "c");

    // Map node name → graph index once. Names are unique per graph.
    let index_of: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();

    let degree = weighted_degree(graph);

    // Collect the strongest strength seen per node across every community it
    // couples (a god-object couples many, so it appears repeatedly).
    let mut best_strength: HashMap<usize, f64> = HashMap::new();
    for community in &analysis.communities {
        for coupler in &community.couplers {
            if coupler.kind != CouplingElementKind::Node {
                continue; // edges are not god-objects
            }
            let Some(name) = coupler.elements.first() else {
                continue;
            };
            let Some(&node) = index_of.get(name.as_str()) else {
                continue;
            };
            let entry = best_strength.entry(node).or_insert(0.0);
            *entry = entry.max(coupler.coupling_strength);
        }
    }

    let mut gods: Vec<GodObject> = best_strength
        .into_iter()
        .filter(|&(node, _)| degree[node] >= min_degree)
        .map(|(node, coupling_strength)| GodObject {
            node,
            coupling_strength,
            degree: degree[node],
        })
        .collect();
    gods.sort_by(|a, b| {
        b.degree
            .partial_cmp(&a.degree)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.node.cmp(&b.node))
    });
    gods
}

/// Weighted degree of every node (sum of incident edge weights).
fn weighted_degree(graph: &Graph) -> Vec<f64> {
    (0..graph.node_count())
        .map(|u| graph.neighbors(u).iter().map(|&(_, w)| w).sum())
        .collect()
}

// ── Community subgraph ────────────────────────────────────────────────────

/// The induced subgraph of one community, kept as an explicit edge list so
/// ablated variants (minus one node or one edge) can be rebuilt cheaply.
struct CommunitySubgraph {
    /// Global node index per local index (ascending).
    globals: Vec<usize>,
    /// Undirected, deduplicated edges in local indices: `(lo, hi, weight)`,
    /// sorted for determinism.
    edges: Vec<(usize, usize, f64)>,
}

impl CommunitySubgraph {
    /// Extract the induced subgraph of `members` (assumed sorted ascending).
    fn extract(graph: &Graph, members: &[usize]) -> Self {
        let local_of: HashMap<usize, usize> =
            members.iter().enumerate().map(|(i, &g)| (g, i)).collect();
        let mut edges: Vec<(usize, usize, f64)> = Vec::new();
        for &gu in members {
            let lu = local_of[&gu];
            for &(gv, w) in graph.neighbors(gu) {
                // Each undirected edge once (gu < gv keeps it deterministic).
                if gu < gv {
                    if let Some(&lv) = local_of.get(&gv) {
                        let (lo, hi) = if lu < lv { (lu, lv) } else { (lv, lu) };
                        edges.push((lo, hi, w));
                    }
                }
            }
        }
        edges.sort_unstable_by_key(|a| (a.0, a.1));
        Self {
            globals: members.to_vec(),
            edges,
        }
    }

    fn n(&self) -> usize {
        self.globals.len()
    }

    /// Build a Leiden [`Graph`], optionally ablating one node (all its
    /// incident edges) or one edge (by index into [`Self::edges`]). An ablated
    /// node keeps its index and becomes isolated, so block membership stays
    /// aligned across variants.
    fn build(&self, without_node: Option<usize>, without_edge: Option<usize>) -> Graph {
        let mut g = Graph::new(self.n());
        for (idx, &(u, v, w)) in self.edges.iter().enumerate() {
            if Some(idx) == without_edge {
                continue;
            }
            if let Some(x) = without_node {
                if u == x || v == x {
                    continue;
                }
            }
            g.add_edge(u, v, w);
        }
        g
    }
}

/// Re-cluster a subgraph at `gamma` with an explicit seed, returning a
/// renumbered partition. The Leiden core only ever merges along edges, so
/// disconnected pieces (e.g. after an ablation) can never share a label — no
/// connectivity post-pass is needed here.
fn cluster(g: &Graph, gamma: f64, seed: u64) -> Vec<usize> {
    let mut p = leiden_core_seeded(g, gamma, seed);
    renumber(&mut p);
    p
}

// ── Step 1: fragility probe ───────────────────────────────────────────────

struct Fragility {
    gamma_hold: f64,
    gamma_split: f64,
    /// Partition of the subgraph at `gamma_split`.
    split_partition: Vec<usize>,
    /// Local indices of the largest block at `gamma_split`.
    block_a: Vec<usize>,
    /// Local indices of the second-largest block.
    block_b: Vec<usize>,
}

/// Walk [`GAMMA_LADDER`] upward on the intact subgraph. Fragile means: some
/// rung where the community holds as one block, followed by a rung where it
/// separates into two blocks of at least [`MIN_BLOCK_SIZE`] nodes each.
/// Returns `None` for communities that never separate (no latent structure)
/// or that separate at every rung (nothing local holds them together — their
/// cohesion is purely an artefact of global context, so there is no local
/// counterfactual to test).
fn probe_fragility(sub: &CommunitySubgraph) -> Option<Fragility> {
    if sub.edges.is_empty() {
        return None;
    }
    let g = sub.build(None, None);
    let mut gamma_hold: Option<f64> = None;

    for &gamma in GAMMA_LADDER {
        let partition = cluster(&g, gamma, PROBE_SEED);
        let blocks = partition.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        if blocks <= 1 {
            gamma_hold = Some(gamma);
            continue;
        }
        if let Some((label_a, label_b)) = two_main_blocks(&partition) {
            let hold = gamma_hold?;
            let block_a: Vec<usize> = collect_block(&partition, label_a);
            let block_b: Vec<usize> = collect_block(&partition, label_b);
            return Some(Fragility {
                gamma_hold: hold,
                gamma_split: gamma,
                split_partition: partition,
                block_a,
                block_b,
            });
        }
        // A trivial separation (a fragment below MIN_BLOCK_SIZE fell off) is
        // neither a hold nor a real split; keep scanning upward.
    }
    None
}

/// The labels of the two largest blocks, provided both have at least
/// [`MIN_BLOCK_SIZE`] nodes. Ties break toward the smaller label so the probe
/// is deterministic.
fn two_main_blocks(partition: &[usize]) -> Option<(usize, usize)> {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for &label in partition {
        *counts.entry(label).or_insert(0) += 1;
    }
    let mut sized: Vec<(usize, usize)> = counts.into_iter().collect();
    // Descending size, ascending label on ties.
    sized.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    match (sized.first(), sized.get(1)) {
        (Some(&(la, ca)), Some(&(lb, cb))) if ca >= MIN_BLOCK_SIZE && cb >= MIN_BLOCK_SIZE => {
            Some((la, lb))
        }
        _ => None,
    }
}

fn collect_block(partition: &[usize], label: usize) -> Vec<usize> {
    partition
        .iter()
        .enumerate()
        .filter(|&(_, &l)| l == label)
        .map(|(i, _)| i)
        .collect()
}

// ── Step 2: cheap candidate scoring ───────────────────────────────────────

/// A candidate coupler awaiting ablation, carrying its proxy scores.
struct Candidate {
    kind: CouplingElementKind,
    /// Local node index (nodes) — also kept for edges as the ablation target.
    node: usize,
    /// Index into `CommunitySubgraph::edges` (edges only).
    edge: Option<usize>,
    participation: f64,
    min_cut_share: f64,
    /// Filled in by verification.
    baseline_split_probability: f64,
    split_probability: f64,
    gamma_low: f64,
    gamma_high: f64,
}

impl Candidate {
    fn coupling_strength(&self) -> f64 {
        self.split_probability - self.baseline_split_probability
    }

    fn into_element(self, sub: &CommunitySubgraph, names: &[String]) -> CouplingElement {
        let elements = match self.edge {
            Some(e) => {
                let (u, v, _) = sub.edges[e];
                vec![names[sub.globals[u]].clone(), names[sub.globals[v]].clone()]
            }
            None => vec![names[sub.globals[self.node]].clone()],
        };
        CouplingElement {
            kind: self.kind,
            elements,
            participation: self.participation,
            min_cut_share: self.min_cut_share,
            baseline_split_probability: self.baseline_split_probability,
            split_probability: self.split_probability,
            coupling_strength: self.coupling_strength(),
            gamma_low: self.gamma_low,
            gamma_high: self.gamma_high,
        }
    }
}

/// Score candidates from the min-cut and participation proxies.
///
/// The A↔B minimum cut is the closed-form glue: its edges are the edge
/// couplers, and a node incident to a large share of the cut is the node
/// coupler. The participation coefficient (how evenly a node's weight spreads
/// across the split partition's blocks) catches hub nodes whose edges the
/// min-cut routed around.
fn score_candidates(sub: &CommunitySubgraph, fragility: &Fragility) -> Vec<Candidate> {
    let n = sub.n();
    let cut = min_cut_edges(sub, &fragility.block_a, &fragility.block_b);
    let cut_total: f64 = cut.iter().map(|&(_, w)| w).sum();

    let mut node_cut_share = vec![0.0f64; n];
    if cut_total > FLOW_EPS {
        for &(e, w) in &cut {
            let (u, v, _) = sub.edges[e];
            node_cut_share[u] += w / cut_total;
            node_cut_share[v] += w / cut_total;
        }
    }
    let participation = participation_coefficients(sub, &fragility.split_partition);

    // Edge candidates: the min-cut edges, heaviest first.
    let mut cut_sorted = cut.clone();
    cut_sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let mut candidates: Vec<Candidate> = cut_sorted
        .into_iter()
        .take(MAX_EDGE_CANDIDATES)
        .map(|(e, w)| {
            let (u, _, _) = sub.edges[e];
            Candidate {
                kind: CouplingElementKind::Edge,
                node: u,
                edge: Some(e),
                participation: 0.0,
                min_cut_share: if cut_total > FLOW_EPS {
                    w / cut_total
                } else {
                    0.0
                },
                baseline_split_probability: 0.0,
                split_probability: 0.0,
                gamma_low: 0.0,
                gamma_high: 0.0,
            }
        })
        .collect();

    // Node candidates: cut incidence + participation, highest combined first.
    let mut nodes: Vec<(usize, f64)> = (0..n)
        .map(|u| (u, node_cut_share[u] + participation[u]))
        .filter(|&(_, score)| score > 0.0)
        .collect();
    nodes.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    candidates.extend(
        nodes
            .into_iter()
            .take(MAX_NODE_CANDIDATES)
            .map(|(u, _)| Candidate {
                kind: CouplingElementKind::Node,
                node: u,
                edge: None,
                participation: participation[u],
                min_cut_share: node_cut_share[u],
                baseline_split_probability: 0.0,
                split_probability: 0.0,
                gamma_low: 0.0,
                gamma_high: 0.0,
            }),
    );
    candidates
}

/// Guimerà–Amaral participation coefficient of every node with respect to
/// `partition`: `P_u = 1 − Σ_s (w_{u,s} / w_u)²`. Zero for nodes whose edges
/// all stay in one block; approaches 1 − 1/k for nodes spread over k blocks.
fn participation_coefficients(sub: &CommunitySubgraph, partition: &[usize]) -> Vec<f64> {
    let n = sub.n();
    let mut weight_to_block: Vec<HashMap<usize, f64>> = vec![Default::default(); n];
    let mut total: Vec<f64> = vec![0.0; n];
    for &(u, v, w) in &sub.edges {
        *weight_to_block[u].entry(partition[v]).or_insert(0.0) += w;
        *weight_to_block[v].entry(partition[u]).or_insert(0.0) += w;
        total[u] += w;
        total[v] += w;
    }
    (0..n)
        .map(|u| {
            if total[u] <= FLOW_EPS {
                return 0.0;
            }
            let sum_sq: f64 = weight_to_block[u]
                .values()
                .map(|&w| (w / total[u]) * (w / total[u]))
                .sum();
            1.0 - sum_sq
        })
        .collect()
}

// ── Min cut (Dinic max-flow) ──────────────────────────────────────────────

/// Arc-list flow network: arcs are stored in pairs so `arc ^ 1` is the
/// reverse arc, the standard residual-graph encoding.
struct FlowNetwork {
    to: Vec<usize>,
    cap: Vec<f64>,
    adj: Vec<Vec<usize>>,
}

impl FlowNetwork {
    fn new(n: usize) -> Self {
        Self {
            to: Vec::new(),
            cap: Vec::new(),
            adj: vec![Vec::new(); n],
        }
    }

    fn add_arc(&mut self, u: usize, v: usize, cap_uv: f64, cap_vu: f64) {
        self.adj[u].push(self.to.len());
        self.to.push(v);
        self.cap.push(cap_uv);
        self.adj[v].push(self.to.len());
        self.to.push(u);
        self.cap.push(cap_vu);
    }

    /// BFS level graph from `s`; `None` level = unreachable in the residual.
    fn levels(&self, s: usize) -> Vec<Option<usize>> {
        let mut level = vec![None; self.adj.len()];
        level[s] = Some(0);
        // Carry each node's level in the queue so no `Option` unwrap is needed.
        let mut queue = VecDeque::from([(s, 0usize)]);
        while let Some((u, lu)) = queue.pop_front() {
            for &a in &self.adj[u] {
                let v = self.to[a];
                if self.cap[a] > FLOW_EPS && level[v].is_none() {
                    level[v] = Some(lu + 1);
                    queue.push_back((v, lu + 1));
                }
            }
        }
        level
    }

    /// DFS blocking flow along strictly increasing levels.
    fn augment(
        &mut self,
        u: usize,
        t: usize,
        pushed: f64,
        level: &[Option<usize>],
        next: &mut [usize],
    ) -> f64 {
        if u == t {
            return pushed;
        }
        while next[u] < self.adj[u].len() {
            let a = self.adj[u][next[u]];
            let v = self.to[a];
            if self.cap[a] > FLOW_EPS && level[v] == level[u].map(|l| l + 1) {
                let flow = self.augment(v, t, pushed.min(self.cap[a]), level, next);
                if flow > FLOW_EPS {
                    self.cap[a] -= flow;
                    self.cap[a ^ 1] += flow;
                    return flow;
                }
            }
            next[u] += 1;
        }
        0.0
    }

    /// Run Dinic from `s` to `t`, then return the residual source-side
    /// reachable set (the min-cut separates it from the rest).
    fn min_cut_reachable(&mut self, s: usize, t: usize) -> Vec<bool> {
        loop {
            let level = self.levels(s);
            if level[t].is_none() {
                return level.iter().map(|l| l.is_some()).collect();
            }
            let mut next = vec![0usize; self.adj.len()];
            loop {
                let flow = self.augment(s, t, f64::INFINITY, &level, &mut next);
                if flow <= FLOW_EPS {
                    break;
                }
            }
        }
    }
}

/// The minimum-cut edge set between the two sub-blocks: block A is contracted
/// into the source, block B into the sink, every subgraph edge carries its
/// weight as capacity in both directions. Returns `(edge index, weight)` for
/// each cut edge.
fn min_cut_edges(
    sub: &CommunitySubgraph,
    block_a: &[usize],
    block_b: &[usize],
) -> Vec<(usize, f64)> {
    let n = sub.n();
    let source = n;
    let sink = n + 1;
    let infinite: f64 = sub.edges.iter().map(|&(_, _, w)| w).sum::<f64>() * 2.0 + 1.0;

    let mut net = FlowNetwork::new(n + 2);
    for &(u, v, w) in &sub.edges {
        net.add_arc(u, v, w, w);
    }
    for &a in block_a {
        net.add_arc(source, a, infinite, 0.0);
    }
    for &b in block_b {
        net.add_arc(b, sink, infinite, 0.0);
    }

    let reachable = net.min_cut_reachable(source, sink);
    sub.edges
        .iter()
        .enumerate()
        .filter(|&(_, &(u, v, _))| reachable[u] != reachable[v])
        .map(|(idx, &(_, _, w))| (idx, w))
        .collect()
}

// ── Steps 3 & 4: ablation verification + γ sweep ──────────────────────────

/// Fraction of [`VERIFY_RUNS`] seeded re-clusterings of `g` at `gamma` in
/// which block A lands apart from block B (by majority label). `skip` is the
/// ablated node, excluded from both blocks' majorities.
fn split_probability(
    g: &Graph,
    gamma: f64,
    block_a: &[usize],
    block_b: &[usize],
    skip: Option<usize>,
) -> f64 {
    let mut splits = 0usize;
    for run in 0..VERIFY_RUNS {
        let partition = cluster(g, gamma, verify_seed(run));
        let ma = majority_label(&partition, block_a, skip);
        let mb = majority_label(&partition, block_b, skip);
        if let (Some(ma), Some(mb)) = (ma, mb) {
            if ma != mb {
                splits += 1;
            }
        }
    }
    splits as f64 / VERIFY_RUNS as f64
}

/// Most common partition label among `block` (minus `skip`); ties break
/// toward the smaller label for determinism.
fn majority_label(partition: &[usize], block: &[usize], skip: Option<usize>) -> Option<usize> {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for &u in block {
        if Some(u) == skip {
            continue;
        }
        *counts.entry(partition[u]).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(label, _)| label)
}

/// Ablate each scored candidate and keep the ones whose removal raises the
/// split probability by at least [`MIN_COUPLING_STRENGTH`] over the intact
/// baseline at `gamma_hold`; sweep the verified ones down the γ ladder to
/// find the resolution range they control.
fn verify_couplers(sub: &CommunitySubgraph, fragility: &Fragility) -> Vec<Candidate> {
    let candidates = score_candidates(sub, fragility);
    if candidates.is_empty() {
        return Vec::new();
    }

    let (block_a, block_b) = (&fragility.block_a, &fragility.block_b);
    // γ rungs at which the intact community held (≤ gamma_hold), ascending.
    let hold_rungs: Vec<f64> = GAMMA_LADDER
        .iter()
        .copied()
        .filter(|&g| g <= fragility.gamma_hold)
        .collect();

    // Intact-subgraph baselines, one per rung (shared by all candidates).
    let intact = sub.build(None, None);
    let baselines: Vec<f64> = hold_rungs
        .iter()
        .map(|&g| split_probability(&intact, g, block_a, block_b, None))
        .collect();
    let hold_idx = hold_rungs.len() - 1;

    let mut verified: Vec<Candidate> = Vec::new();
    for mut candidate in candidates {
        let (skip, ablated) = match candidate.edge {
            Some(e) => (None, sub.build(None, Some(e))),
            None => (Some(candidate.node), sub.build(Some(candidate.node), None)),
        };

        candidate.split_probability =
            split_probability(&ablated, fragility.gamma_hold, block_a, block_b, skip);
        candidate.baseline_split_probability = baselines[hold_idx];
        if candidate.coupling_strength() < MIN_COUPLING_STRENGTH {
            continue;
        }

        // γ sweep: the contiguous-ish range of rungs the coupler controls —
        // where the intact community holds but the ablated one splits.
        let mut active: Vec<f64> = Vec::new();
        for (i, &gamma) in hold_rungs.iter().enumerate() {
            let ablated_prob = if i == hold_idx {
                candidate.split_probability
            } else {
                split_probability(&ablated, gamma, block_a, block_b, skip)
            };
            if baselines[i] < ACTIVE_SPLIT_PROBABILITY && ablated_prob >= ACTIVE_SPLIT_PROBABILITY {
                active.push(gamma);
            }
        }
        // A coupler can clear the strength threshold while hovering just
        // under the "active" cutoff at every rung; anchor its range at
        // gamma_hold, where it was verified.
        candidate.gamma_low = active.first().copied().unwrap_or(fragility.gamma_hold);
        candidate.gamma_high = active.last().copied().unwrap_or(fragility.gamma_hold);
        verified.push(candidate);
    }

    // Strongest first; proxies then element identity break ties.
    verified.sort_by(|a, b| {
        b.coupling_strength()
            .partial_cmp(&a.coupling_strength())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.min_cut_share
                    .partial_cmp(&a.min_cut_share)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.node.cmp(&b.node))
    });
    verified
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `size`-clique on consecutive nodes starting at `base`.
    fn add_clique(g: &mut Graph, base: usize, size: usize) {
        for i in base..base + size {
            for j in (i + 1)..base + size {
                g.add_edge(i, j, 1.0);
            }
        }
    }

    fn fake_names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("src/file_{i}.rs")).collect()
    }

    /// Two 5-cliques joined by one bridge edge (0 ↔ 5).
    fn bridged_cliques() -> Graph {
        let mut g = Graph::new(10);
        add_clique(&mut g, 0, 5);
        add_clique(&mut g, 5, 5);
        g.add_edge(0, 5, 1.0);
        g
    }

    /// Two 5-cliques whose only connection is a hub node (10) wired to every
    /// node of both cliques.
    fn hub_cliques() -> Graph {
        let mut g = Graph::new(11);
        add_clique(&mut g, 0, 5);
        add_clique(&mut g, 5, 5);
        for i in 0..10 {
            g.add_edge(10, i, 1.0);
        }
        g
    }

    /// Run the per-community pipeline on the whole graph as one community.
    fn analyze_whole(g: &Graph) -> Option<(Fragility, Vec<Candidate>)> {
        let members: Vec<usize> = (0..g.node_count()).collect();
        let sub = CommunitySubgraph::extract(g, &members);
        let fragility = probe_fragility(&sub)?;
        let couplers = verify_couplers(&sub, &fragility);
        Some((fragility, couplers))
    }

    #[test]
    fn test_bridge_edge_is_detected_as_coupler() {
        let g = bridged_cliques();
        let (fragility, couplers) = analyze_whole(&g).expect("two bridged cliques are fragile");

        // The latent sub-blocks are the two cliques.
        assert_eq!(fragility.block_a.len(), 5);
        assert_eq!(fragility.block_b.len(), 5);
        assert!(fragility.gamma_hold < fragility.gamma_split);

        // The bridge edge 0↔5 must be among the verified couplers, carrying
        // the whole min cut.
        let edge = couplers
            .iter()
            .find(|c| c.kind == CouplingElementKind::Edge)
            .expect("bridge edge verified as coupler");
        let (u, v, _) = {
            let members: Vec<usize> = (0..g.node_count()).collect();
            let sub = CommunitySubgraph::extract(&g, &members);
            sub.edges[edge.edge.unwrap()]
        };
        assert_eq!((u, v), (0, 5));
        assert!((edge.min_cut_share - 1.0).abs() < 1e-9);
        assert!(edge.coupling_strength() >= MIN_COUPLING_STRENGTH);
        // Removing the bridge disconnects the cliques: every seeded run splits.
        assert!((edge.split_probability - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_hub_node_is_detected_as_coupler() {
        let g = hub_cliques();
        let (fragility, couplers) = analyze_whole(&g).expect("hub-joined cliques are fragile");

        // The hub carries the whole cut and tops the participation ranking,
        // so it must be the strongest verified node coupler.
        let node = couplers
            .iter()
            .find(|c| c.kind == CouplingElementKind::Node)
            .expect("hub verified as node coupler");
        assert_eq!(node.node, 10);
        assert!(node.participation > 0.4, "hub spans both blocks");
        assert!(node.coupling_strength() >= MIN_COUPLING_STRENGTH);
        assert!((node.split_probability - 1.0).abs() < 1e-9);

        // Sanity: the two cliques are the latent blocks (hub lands in one).
        let mut sizes = [fragility.block_a.len(), fragility.block_b.len()];
        sizes.sort_unstable();
        assert!(sizes[0] == 5 && (sizes[1] == 5 || sizes[1] == 6));
    }

    #[test]
    fn test_single_clique_has_no_couplers() {
        let mut g = Graph::new(6);
        add_clique(&mut g, 0, 6);
        assert!(
            analyze_whole(&g).is_none(),
            "a clique has no latent 2-block structure"
        );
    }

    #[test]
    fn test_analyze_graph_reports_fragile_community() {
        // Two bridged 5-cliques plus a detached 4-clique: the baseline
        // partition separates the detached clique, and only the bridged pair
        // is reported as fragile.
        let mut g = Graph::new(14);
        add_clique(&mut g, 0, 5);
        add_clique(&mut g, 5, 5);
        g.add_edge(0, 5, 1.0);
        add_clique(&mut g, 10, 4);
        let names = fake_names(14);

        let analysis = analyze_graph(&g, &names, "c");
        // Baseline may or may not keep the bridged cliques merged; either way
        // the detached clique is never fragile and never gains couplers.
        assert!(analysis.total_communities >= 2);
        for community in &analysis.communities {
            for coupler in &community.couplers {
                for element in &coupler.elements {
                    let idx: usize = element
                        .trim_start_matches("src/file_")
                        .trim_end_matches(".rs")
                        .parse()
                        .unwrap();
                    assert!(idx < 10, "couplers only in the bridged pair: {element}");
                }
            }
        }
    }

    #[test]
    fn test_analysis_is_deterministic() {
        let g = hub_cliques();
        let names = fake_names(11);
        let first = analyze_graph(&g, &names, "c");
        let second = analyze_graph(&g, &names, "c");
        assert_eq!(first.communities, second.communities);
        assert_eq!(first.fragile_communities, second.fragile_communities);
    }

    #[test]
    fn test_min_cut_finds_bridge() {
        let g = bridged_cliques();
        let members: Vec<usize> = (0..10).collect();
        let sub = CommunitySubgraph::extract(&g, &members);
        let block_a: Vec<usize> = (0..5).collect();
        let block_b: Vec<usize> = (5..10).collect();
        let cut = min_cut_edges(&sub, &block_a, &block_b);
        assert_eq!(cut.len(), 1);
        let (u, v, w) = sub.edges[cut[0].0];
        assert_eq!((u, v), (0, 5));
        assert!((w - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_participation_coefficient() {
        // Path a–b–c partitioned as {a} | {b, c}: `a` sends everything to the
        // other block (P > 0), `c` keeps everything inside its own (P = 0).
        let mut g = Graph::new(3);
        g.add_edge(0, 1, 1.0);
        g.add_edge(1, 2, 1.0);
        let sub = CommunitySubgraph::extract(&g, &[0, 1, 2]);
        let p = participation_coefficients(&sub, &[0, 1, 1]);
        assert_eq!(p[0], 0.0); // all of a's weight goes to one block (b's)
        assert!(p[1] > 0.0); // b splits its weight across both blocks
        assert_eq!(p[2], 0.0);
    }
}
