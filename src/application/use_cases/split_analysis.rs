use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::domain::{ConsumerRepo, DomainError, FileGraph, SplitAnalysis, SplitGroup};

use super::FileRelationshipUseCase;

/// Use case that analyses a "monolith" repository and identifies natural
/// extraction boundaries by looking at which external consumers reference which
/// subsets of its files.
///
/// Algorithm
/// ---------
/// 1. Build the complete file-dependency graph across all repositories.
/// 2. Extract **inbound cross-repo edges**: edges where `to_repo_id == target`
///    and `from_repo_id != target`.  These edges identify files in the monolith
///    that form its externally-visible public interface.
/// 3. For every target file in that public interface, compute its **consumer
///    fingerprint** — the sorted set of external `repo_id`s that reference it.
/// 4. Group target files that share the same consumer fingerprint — these
///    naturally belong together in an extracted library.
/// 5. For each group, find **support files**: files inside the monolith that are
///    only referenced by public-interface files belonging to that group (and not
///    by any external repo directly).  These would travel with the extracted
///    library.
/// 6. Return [`SplitAnalysis`].
pub struct SplitAnalysisUseCase {
    file_relationship: Arc<FileRelationshipUseCase>,
}

impl SplitAnalysisUseCase {
    pub fn new(file_relationship: Arc<FileRelationshipUseCase>) -> Self {
        Self { file_relationship }
    }

    pub async fn analyse(&self, target_repo_id: &str) -> Result<SplitAnalysis, DomainError> {
        // ── 1. Build the full graph (all repos, no min-weight filter) ─────────
        let graph = self
            .file_relationship
            .build_graph(None, 1, true)
            .await?;

        self.analyse_from_graph(target_repo_id, &graph)
    }

    /// Performs the analysis from a pre-built [`FileGraph`].
    /// Separated to allow callers to reuse an existing graph.
    pub fn analyse_from_graph(
        &self,
        target_repo_id: &str,
        graph: &FileGraph,
    ) -> Result<SplitAnalysis, DomainError> {
        let target_repo = graph
            .repositories
            .get(target_repo_id)
            .ok_or_else(|| {
                DomainError::not_found(format!(
                    "Repository '{}' not found in the graph. \
                     Make sure it has been indexed.",
                    target_repo_id
                ))
            })?;

        // ── 2. Inbound cross-repo edges (external → target) ──────────────────
        // Map: target_file → set of external repo_ids
        let mut public_interface: HashMap<String, HashSet<String>> = HashMap::new();
        for edge in &graph.edges {
            if edge.to_repo_id == target_repo_id && edge.from_repo_id != target_repo_id {
                public_interface
                    .entry(edge.to_file.clone())
                    .or_default()
                    .insert(edge.from_repo_id.clone());
            }
        }

        // ── 3. Consumer fingerprint → group ──────────────────────────────────
        // fingerprint (sorted vec) → list of target files
        let mut fingerprint_to_files: HashMap<Vec<String>, Vec<String>> = HashMap::new();
        for (file, consumers) in &public_interface {
            let mut fp: Vec<String> = consumers.iter().cloned().collect();
            fp.sort();
            fingerprint_to_files
                .entry(fp)
                .or_default()
                .push(file.clone());
        }

        // ── 4. Intra-target edges (target → target) for support-file detection
        // Map: target_file → set of target_files it is referenced by
        let mut internal_referenced_by: HashMap<String, HashSet<String>> = HashMap::new();
        let public_files_all: HashSet<String> = public_interface.keys().cloned().collect();
        for edge in &graph.edges {
            if edge.from_repo_id == target_repo_id && edge.to_repo_id == target_repo_id {
                // from_file → to_file means from_file depends on to_file
                internal_referenced_by
                    .entry(edge.to_file.clone())
                    .or_default()
                    .insert(edge.from_file.clone());
            }
        }

        // ── 5. Build SplitGroups ──────────────────────────────────────────────
        let mut groups: Vec<SplitGroup> = fingerprint_to_files
            .into_iter()
            .map(|(consumers, mut pub_files)| {
                pub_files.sort();
                let pub_set: HashSet<&str> = pub_files.iter().map(|s| s.as_str()).collect();

                // Support files: internal files referenced ONLY by files in this
                // group's public interface (not by external repos or other groups).
                let support_files: Vec<String> = {
                    let mut sv: Vec<String> = internal_referenced_by
                        .iter()
                        .filter(|(candidate, referencers)| {
                            // candidate must not itself be in the external public interface
                            !public_files_all.contains(*candidate)
                                // all referencers of this candidate must be in this group's
                                // public interface
                                && referencers.iter().all(|r| pub_set.contains(r.as_str()))
                        })
                        .map(|(f, _)| f.clone())
                        .collect();
                    sv.sort();
                    sv
                };

                // Stable ID: hash of sorted consumers list
                let id = consumers.join("+").replace('/', "_").replace('.', "_");
                // Label: consumer repo names (fall back to ID when name unavailable)
                let label = consumers
                    .iter()
                    .map(|cid| {
                        graph
                            .repositories
                            .get(cid)
                            .map(|r| r.name.as_str())
                            .unwrap_or(cid.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");

                SplitGroup {
                    id,
                    label,
                    consumers,
                    public_files: pub_files,
                    support_files,
                }
            })
            .collect();

        // Sort: groups with most public files first, then by id for stability
        groups.sort_by(|a, b| {
            b.public_files
                .len()
                .cmp(&a.public_files.len())
                .then(a.id.cmp(&b.id))
        });

        // ── 6. Consumer repo metadata ─────────────────────────────────────────
        let all_consumer_ids: HashSet<String> = groups
            .iter()
            .flat_map(|g| g.consumers.iter().cloned())
            .collect();
        let consumers: HashMap<String, ConsumerRepo> = all_consumer_ids
            .into_iter()
            .filter_map(|cid| {
                graph.repositories.get(&cid).map(|r| {
                    (
                        cid.clone(),
                        ConsumerRepo {
                            id: cid,
                            name: r.name.clone(),
                            path: r.path.clone(),
                        },
                    )
                })
            })
            .collect();

        // ── 7. Aggregate stats ────────────────────────────────────────────────
        let total_files_in_target = graph
            .files
            .iter()
            .filter(|f| {
                // A file belongs to the target repo if any edge references it with target repo_id
                graph.edges.iter().any(|e| {
                    (e.from_file == **f && e.from_repo_id == target_repo_id)
                        || (e.to_file == **f && e.to_repo_id == target_repo_id)
                })
            })
            .count();

        let externally_visible_count = public_interface.len();

        Ok(SplitAnalysis {
            target_repo_id: target_repo_id.to_string(),
            target_repo_name: target_repo.name.clone(),
            target_repo_path: target_repo.path.clone(),
            groups,
            consumers,
            total_files_in_target,
            externally_visible_count,
        })
    }
}
