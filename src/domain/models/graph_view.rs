//! A render-ready view of a Leiden-clustered graph.
//!
//! Both the file-level ([`crate::domain::ClusterGraph`]) and symbol-level
//! ([`crate::domain::SymbolCommunityGraph`]) detectors already build the full
//! graph — nodes, edges, and the community partition — while computing their
//! summaries. A [`GraphView`] is that underlying graph exposed in a form a
//! renderer can turn into an interactive HTML page, an SVG, GraphML, or an
//! Obsidian canvas. It carries no logic; the rendering lives in
//! `application::use_cases::visualize_graph`.

use serde::{Deserialize, Serialize};

/// Which graph a [`GraphView`] describes — affects only titles and labelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphLevel {
    /// File-dependency graph (architectural modules).
    File,
    /// Symbol call graph (behavioural communities).
    Symbol,
}

impl GraphLevel {
    /// Human-readable noun for the nodes at this level ("file" / "symbol").
    pub fn node_noun(&self) -> &'static str {
        match self {
            GraphLevel::File => "file",
            GraphLevel::Symbol => "symbol",
        }
    }

    /// Parse the wire representation accepted by the CLI, MCP, and REST
    /// surfaces. Keeping the accepted values and the error message in one place
    /// prevents the adapters from drifting apart. On an unknown value returns
    /// the canonical error message (without the offending value, so callers can
    /// prepend it in whatever style their surface uses).
    pub fn parse(s: &str) -> Result<Self, &'static str> {
        match s {
            "file" => Ok(GraphLevel::File),
            "symbol" => Ok(GraphLevel::Symbol),
            _ => Err("expected 'file' or 'symbol'"),
        }
    }
}

/// A single node: a file path or a fully-qualified symbol name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    /// Stable identifier — the file path or symbol FQN.
    pub id: String,
    /// Short display label (basename / short symbol name).
    pub label: String,
    /// Index of the community this node belongs to (matches [`CommunityMeta::index`]).
    pub community: usize,
    /// Number of incident edges — drives node size in the rendered output.
    pub degree: usize,
    /// Dominant language of this node, lowercased (e.g. "rust", "python").
    pub language: String,
}

/// A single undirected edge between two [`GraphNode`]s, referenced by index
/// into [`GraphView::nodes`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source node index.
    pub source: usize,
    /// Target node index.
    pub target: usize,
    /// Composite edge weight (coupling strength).
    pub weight: f64,
    /// Dominant reference kind for this edge, if known (e.g. "call", "import").
    pub kind: Option<String>,
}

/// Per-community metadata: index, generated name, size, and cohesion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityMeta {
    /// Community index (0-based; `node.community` keys into this).
    pub index: usize,
    /// Human-readable name derived from member names.
    pub name: String,
    /// Number of member nodes.
    pub size: usize,
    /// Ratio of internal to total incident edges.
    pub cohesion: f32,
}

/// The full render-ready graph for one repository at one level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphView {
    pub repository_id: String,
    pub level: GraphLevel,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Communities, ordered by descending size (index need not equal position).
    pub communities: Vec<CommunityMeta>,
}

impl GraphView {
    /// Total node count.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total edge count.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::GraphLevel;

    #[test]
    fn graph_level_parse_accepts_known_values() {
        assert_eq!(GraphLevel::parse("file"), Ok(GraphLevel::File));
        assert_eq!(GraphLevel::parse("symbol"), Ok(GraphLevel::Symbol));
    }

    #[test]
    fn graph_level_parse_rejects_unknown_values() {
        assert!(GraphLevel::parse("bogus").is_err());
        // Case-sensitive: the wire form is lowercase (matches serde).
        assert!(GraphLevel::parse("File").is_err());
        assert!(GraphLevel::parse("").is_err());
    }

    #[test]
    fn graph_level_parse_round_trips_node_noun() {
        for level in [GraphLevel::File, GraphLevel::Symbol] {
            assert_eq!(GraphLevel::parse(level.node_noun()), Ok(level));
        }
    }
}
