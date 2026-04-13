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
