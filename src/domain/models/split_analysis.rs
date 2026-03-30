use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One extraction candidate — a set of files in the target repository that are
/// referenced by the same cohort of external consumers and therefore belong
/// together in a potential extracted library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitGroup {
    /// Stable identifier derived from the sorted consumer list.
    pub id: String,
    /// Human-readable label: names of the consumer repositories.
    pub label: String,
    /// External repository IDs that reference at least one file in this group.
    pub consumers: Vec<String>,
    /// Files in the target repo that are directly referenced by external repos.
    pub public_files: Vec<String>,
    /// Files in the target repo that are only referenced by the public_files in
    /// this group (internal support code that would travel with the extraction).
    pub support_files: Vec<String>,
}

impl SplitGroup {
    pub fn total_files(&self) -> usize {
        self.public_files.len() + self.support_files.len()
    }
}

/// Lightweight description of an external consumer repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerRepo {
    pub id: String,
    pub name: String,
    pub path: String,
}

/// Full monolith-splitting analysis for a target repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitAnalysis {
    /// The repository being analysed (the monolith).
    pub target_repo_id: String,
    pub target_repo_name: String,
    pub target_repo_path: String,
    /// Extraction candidate groups, ordered by descending public_file count.
    pub groups: Vec<SplitGroup>,
    /// External repositories keyed by ID.
    pub consumers: HashMap<String, ConsumerRepo>,
    /// Total number of distinct files in the target repository (from edge data).
    pub total_files_in_target: usize,
    /// Number of files that are referenced by at least one external consumer.
    pub externally_visible_count: usize,
}

impl SplitAnalysis {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}
