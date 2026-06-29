use serde::{Deserialize, Serialize};

/// A named group of files that are tightly coupled to each other and loosely
/// coupled to the rest of the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    /// UUID identifying this cluster.
    pub id: String,
    /// Human-readable name derived from common directory/symbol patterns.
    pub name: String,
    /// Repository this cluster belongs to.
    pub repository_id: String,
    /// Most common programming language among member files.
    pub dominant_language: String,
    /// Number of member files.
    pub size: usize,
    /// Ratio of actual internal edges to all edges touching this cluster.
    /// `internal_edges / (internal_edges + external_edges)`
    pub cohesion: f32,
    /// File paths of all member files, sorted alphabetically.
    pub members: Vec<String>,
}

/// The full cluster graph for a repository — the result of running Leiden
/// detection on the file dependency graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterGraph {
    /// All detected clusters, sorted by descending size.
    pub clusters: Vec<Cluster>,
    pub repository_id: String,
    /// Total number of file nodes in the underlying graph.
    pub total_files: usize,
    /// Total number of file-level dependency edges in the underlying graph.
    pub total_edges: usize,
}

/// A named community of symbols (functions, methods, types) that call or
/// reference one another tightly — the result of running Leiden on the
/// **symbol-level** call graph rather than the file-level dependency graph.
///
/// Where a [`Cluster`] groups files into architectural modules, a
/// `SymbolCommunity` groups individual symbols into behavioural units (a feature,
/// a subsystem, a collaborating set of functions) that can cut across files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolCommunity {
    /// UUID identifying this community.
    pub id: String,
    /// Human-readable name derived from common tokens in member symbol names.
    pub name: String,
    /// Repository this community belongs to.
    pub repository_id: String,
    /// Most common programming language among member symbols.
    pub dominant_language: String,
    /// Number of member symbols.
    pub size: usize,
    /// Ratio of internal edges to all edges touching this community:
    /// `internal_edges / (internal_edges + external_edges)`.
    pub cohesion: f32,
    /// Fully-qualified names of all member symbols, sorted alphabetically.
    pub members: Vec<String>,
}

/// The full symbol-community graph for a repository — the result of running
/// Leiden detection on the symbol-level call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolCommunityGraph {
    /// All detected communities, sorted by descending size.
    pub communities: Vec<SymbolCommunity>,
    pub repository_id: String,
    /// Total number of symbol nodes in the underlying call graph.
    pub total_symbols: usize,
    /// Total number of symbol-level edges in the underlying call graph.
    pub total_edges: usize,
}
