//! Coupling elements: the glue holding internally-fragile Leiden communities
//! together.
//!
//! A *coupling element* is defined counterfactually: a node or edge whose
//! removal makes a single detected community split into two sub-blocks that
//! were latent inside it all along. These types carry the result of that
//! analysis (see `application::use_cases::coupling_detection`): per fragile
//! community, the two sub-blocks, the resolution range over which the merge is
//! controlled, and the ranked, ablation-verified coupling elements.

use serde::{Deserialize, Serialize};

use super::GraphLevel;

/// Whether a coupling element is a node (file/symbol) or an edge (dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CouplingElementKind {
    /// A single file or symbol whose removal splits the community.
    Node,
    /// A single dependency edge whose removal splits the community.
    Edge,
}

/// One verified coupling element inside a fragile community.
///
/// All probabilities are estimated over a fixed set of seeded Leiden re-runs
/// of the community's induced subgraph (Leiden is stochastic, so a single
/// run's outcome is noise; the *fraction* of runs that split is the signal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouplingElement {
    /// Node vs edge.
    pub kind: CouplingElementKind,
    /// The node id (file path / symbol FQN) — or, for an edge, its two
    /// endpoint ids.
    pub elements: Vec<String>,
    /// Guimerà–Amaral participation coefficient of the node with respect to
    /// the community's latent sub-blocks: `1 − Σ_s (k_s/k)²`. High values mean
    /// the node's edges span both blocks. Always `0.0` for edges.
    pub participation: f64,
    /// Share of the A↔B minimum cut carried by this element (for a node, the
    /// summed share of its incident cut edges). The min-cut edge set *is* the
    /// glue between the two sub-blocks, so a high share marks the element that
    /// concentrates it.
    pub min_cut_share: f64,
    /// Fraction of seeded re-clusterings that split A from B *without*
    /// removing this element (the control group).
    pub baseline_split_probability: f64,
    /// Fraction of seeded re-clusterings that split A from B *after* removing
    /// this element.
    pub split_probability: f64,
    /// `split_probability − baseline_split_probability`: how much removing
    /// this element alone raises the chance the community falls apart.
    pub coupling_strength: f64,
    /// Lowest resolution (γ) at which this element controls the A/B merge.
    pub gamma_low: f64,
    /// Highest resolution (γ) at which this element controls the A/B merge.
    pub gamma_high: f64,
}

/// A community that is internally two latent sub-blocks, together with the
/// coupling elements verified (by ablation) to hold it together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommunityCoupling {
    /// Stable, content-addressed community id — matches the id shown by the
    /// `clusters` / `symbol-clusters` commands for the same member set.
    pub community_id: String,
    /// Number of member nodes in the community.
    pub size: usize,
    /// Largest probed resolution at which the community's induced subgraph
    /// still holds together as a single block.
    pub gamma_hold: f64,
    /// Smallest probed resolution at which it separates into the sub-blocks
    /// below. Between `gamma_hold` and `gamma_split` lies the community's
    /// internal fault line.
    pub gamma_split: f64,
    /// Members of the larger latent sub-block, sorted.
    pub sub_block_a: Vec<String>,
    /// Members of the smaller latent sub-block, sorted.
    pub sub_block_b: Vec<String>,
    /// Verified coupling elements, strongest first.
    pub couplers: Vec<CouplingElement>,
}

/// The full coupling analysis for a repository at one graph level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouplingReport {
    pub repository_id: String,
    /// Which graph was analysed: the file-dependency graph or the symbol call
    /// graph.
    pub level: GraphLevel,
    /// Communities in the baseline Leiden partition.
    pub total_communities: usize,
    /// Communities whose induced subgraph revealed latent 2-block structure.
    pub fragile_communities: usize,
    /// Fragile communities with at least one verified coupling element,
    /// ordered by their strongest coupler.
    pub communities: Vec<CommunityCoupling>,
}
