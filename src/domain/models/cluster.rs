use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Scope-id prefix under which namespace-wide (cross-repository) analyses are
/// stored.
///
/// The analysis cache is keyed by `repository_id`; a namespace-wide Leiden run
/// spans every repository in the namespace, so it is cached under a sentinel
/// scope id instead of any single repository's id. The cache table has no
/// namespace column and is shared across every namespace, so the sentinel must
/// carry the namespace itself (see [`namespace_scope_id`]) — otherwise two
/// namespaces' global runs would overwrite each other under one bare key.
/// Repository ids are UUIDs, so the sentinel can never collide with a real one.
/// Any re-index or repository deletion invalidates the sentinel entry alongside
/// the repository's own, since the global graph derives from every repository's
/// call graph.
pub const NAMESPACE_SCOPE_ID: &str = "__namespace__";

/// Cache scope id for the namespace-wide analysis of `namespace`.
///
/// Combines [`NAMESPACE_SCOPE_ID`] with the namespace so each namespace gets its
/// own slot in the (namespace-less) analysis cache. A UUID repository id can
/// never take this shape, so the key stays disjoint from per-repository ones.
pub fn namespace_scope_id(namespace: &str) -> String {
    format!("{NAMESPACE_SCOPE_ID}:{namespace}")
}

/// Derive a stable, content-addressed community id from its members.
///
/// The id is a short hex digest of the (already sorted) member list, so the
/// *same* set of files/symbols always yields the *same* id across re-index and
/// recompute runs — unlike a random UUID, which changes every time. This is what
/// lets an expensive, cached artefact (an LLM-generated display name) be keyed on
/// a community and survive recomputation as long as its membership is unchanged.
///
/// `prefix` distinguishes the two levels (`c` for file clusters, `s` for symbol
/// communities) so ids never collide between them.
pub fn stable_community_id(prefix: &str, members: &[String]) -> String {
    let mut hasher = Sha256::new();
    for member in members {
        hasher.update(member.as_bytes());
        hasher.update([0u8]); // delimiter so ["ab","c"] ≠ ["a","bc"]
    }
    let digest = hasher.finalize();
    // 12 hex chars (48 bits) is ample to avoid collisions within one repo's
    // few-hundred communities while staying short enough to show in a listing.
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

/// The user-facing label for a community: its LLM display name when one has been
/// generated, otherwise its stable id. Every render path uses this so the same
/// community reads identically in `list`, `get`, `overview`, MCP, and the
/// management API (a bare id until it is named, never a blank).
pub fn community_label<'a>(display_name: &'a Option<String>, id: &'a str) -> &'a str {
    display_name.as_deref().unwrap_or(id)
}

/// A named group of files that are tightly coupled to each other and loosely
/// coupled to the rest of the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    /// Stable, content-addressed id (see [`stable_community_id`]).
    pub id: String,
    /// LLM-generated human-readable name, cached by [`Self::id`]. `None` until a
    /// name has been generated (or when no LLM is configured); callers then show
    /// the id via [`community_label`].
    #[serde(default)]
    pub display_name: Option<String>,
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
    /// Stable, content-addressed id (see [`stable_community_id`]).
    pub id: String,
    /// LLM-generated human-readable name, cached by [`Self::id`]. `None` until a
    /// name has been generated (or when no LLM is configured); callers then show
    /// the id via [`community_label`].
    #[serde(default)]
    pub display_name: Option<String>,
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
