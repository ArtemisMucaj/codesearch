use serde::{Deserialize, Serialize};

/// A single node in the forward call chain of an execution feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureNode {
    /// The fully-qualified symbol name.
    pub symbol: String,
    /// Source file where this symbol is declared.
    pub file_path: String,
    /// Line number where the reference occurs.
    pub line: u32,
    /// BFS depth from the entry point (0 = entry point itself).
    pub depth: usize,
    /// Repository that contains this symbol.
    pub repository_id: String,
}

/// An execution feature: a named forward call chain starting from an entry-point
/// symbol, annotated with a criticality score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionFeature {
    /// Stable identifier derived from the entry-point symbol and repository.
    pub id: String,
    /// Human-readable label (short name of the entry-point symbol).
    pub name: String,
    /// Fully-qualified entry-point symbol name.
    pub entry_point: String,
    /// Repository this feature belongs to.
    pub repository_id: String,
    /// Ordered call chain starting at the entry point (index 0) and tracing
    /// forward through callees via BFS.
    pub path: Vec<FeatureNode>,
    /// `len(path) - 1` — number of hops from entry point to deepest reachable symbol.
    pub depth: usize,
    /// Number of distinct source files touched by the call chain.
    pub file_count: usize,
    /// Composite criticality score in the range 0.0–1.0.  Higher values indicate
    /// paths that are more important to understand before making changes.
    pub criticality: f32,
}
