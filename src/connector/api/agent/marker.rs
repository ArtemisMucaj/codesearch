//! The per-project "indexed" marker.
//!
//! `codesearch index` drops a small `.codesearch/project.json` file in the root
//! of the repository it indexed. The agent hooks installed by `codesearch
//! install` consult this marker to decide whether to nudge: a nudge only fires
//! when the current working tree has actually been indexed, so freshly cloned or
//! never-indexed repositories are never spammed. This mirrors how graphify gates
//! its hooks on `graphify-out/graph.json`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Directory (relative to a repository root) that holds the marker.
pub const MARKER_DIR: &str = ".codesearch";
/// File name of the marker inside [`MARKER_DIR`].
pub const MARKER_FILE: &str = "project.json";

/// Contents of `.codesearch/project.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMarker {
    /// Repository ID assigned by the indexer.
    pub repository_id: String,
    /// Human-readable repository name.
    pub name: String,
    /// Namespace the repository was indexed under, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Unix timestamp (seconds) of the most recent index.
    pub indexed_at: u64,
}

impl ProjectMarker {
    /// Build a marker, stamping it with the current time.
    pub fn new(repository_id: String, name: String, namespace: Option<String>) -> Self {
        let indexed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            repository_id,
            name,
            namespace,
            indexed_at,
        }
    }
}

/// Absolute path of the marker for a given repository root.
pub fn marker_path(root: &Path) -> PathBuf {
    root.join(MARKER_DIR).join(MARKER_FILE)
}

/// Write (or overwrite) the marker under `root`, returning its path.
pub fn write_marker(root: &Path, marker: &ProjectMarker) -> std::io::Result<PathBuf> {
    let dir = root.join(MARKER_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(MARKER_FILE);
    let json = serde_json::to_string_pretty(marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Walk up from `start` looking for a `.codesearch/project.json` marker. Returns
/// the path of the first marker found, or `None` if none exists in any ancestor.
pub fn find_marker(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = marker_path(dir);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_find_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let marker = ProjectMarker::new("repo-1".into(), "demo".into(), Some("search".into()));
        let written = write_marker(root, &marker).unwrap();
        assert!(written.is_file());

        // Found from the root itself…
        assert_eq!(find_marker(root).as_deref(), Some(written.as_path()));
        // …and from a nested subdirectory.
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_marker(&nested).as_deref(), Some(written.as_path()));
    }

    #[test]
    fn find_marker_absent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_marker(tmp.path()).is_none());
    }
}
