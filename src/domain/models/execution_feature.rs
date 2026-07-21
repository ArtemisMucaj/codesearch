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
    /// The symbol whose call discovered this node — its BFS parent. `None` for
    /// the entry point. Lets clients fold the flat path into a call tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// Total execution callees of this symbol in the call graph. May exceed the
    /// node's child count in the folded tree: the BFS visits each symbol once,
    /// under its FIRST discoverer, so callees first reached elsewhere don't
    /// reappear here. Lets clients tell a true leaf from a deduplicated one.
    #[serde(default)]
    pub callee_count: usize,
    /// Display name of the repository this SYMBOL lives in, set only when it
    /// differs from the feature's own repository — i.e. the flow crossed into
    /// a namespace sibling. `None` for same-repo nodes and for leaves, whose
    /// owning repo can't be inferred (a symbol's home is read off its outgoing
    /// call edges; a leaf has none). `file_path`/`line` remain the CALL SITE,
    /// which lives in the caller's repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_name: Option<String>,
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
    /// Number of distinct symbols transitively reachable from the entry point
    /// (including the entry point itself) over real call edges. This is the
    /// primary measure of "how much of a flow" the feature is — deep, widely
    /// connected features reach many symbols; leaf methods reach only a few.
    pub reach: usize,
    /// Composite criticality score in the range 0.0–1.0.  Higher values indicate
    /// paths that are more important to understand before making changes.
    /// Dominated by transitive reachability and call-chain depth.
    pub criticality: f32,
}
