use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// A directed edge in the file dependency graph: file A depends on (calls into) file B.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdge {
    /// File that contains the references (the dependent).
    pub from_file: String,
    /// Repository the dependent file belongs to.
    pub from_repo_id: String,
    /// File that contains the referenced symbols (the dependency).
    pub to_file: String,
    /// Repository the dependency file belongs to.
    pub to_repo_id: String,
    /// Number of distinct symbol references from `from_file` into `to_file`.
    pub weight: usize,
    /// Distinct reference kinds contributing to this edge (e.g. "Call", "Import").
    pub reference_kinds: Vec<String>,
    /// Distinct callee symbol names from `to_file` that are referenced by `from_file`.
    pub symbols: Vec<String>,
}

impl FileEdge {
    /// Returns true when both endpoints belong to different repositories.
    pub fn is_cross_repo(&self) -> bool {
        self.from_repo_id != self.to_repo_id
    }
}

/// Lightweight metadata about an indexed repository, used for graph annotations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGraphRepo {
    pub id: String,
    pub name: String,
    pub path: String,
}

/// A file-level dependency graph across one or more repositories.
///
/// Each node is a source file path; each edge is a directional dependency
/// (from_file calls symbols defined in to_file).  Repositories are exposed as
/// a map so renderers can draw cluster boundaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGraph {
    /// All repositories whose files appear in the graph.
    pub repositories: HashMap<String, FileGraphRepo>,
    /// All distinct file paths that appear as nodes (sources or targets).
    pub files: HashSet<String>,
    /// Dependency edges, sorted by descending weight.
    pub edges: Vec<FileEdge>,
}

impl FileGraph {
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Total number of dependency edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Total number of file nodes.
    pub fn node_count(&self) -> usize {
        self.files.len()
    }
}
