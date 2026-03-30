use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::application::{CallGraphUseCase, MetadataRepository, VectorRepository};
use crate::domain::{DomainError, FileEdge, FileGraph, FileGraphRepo};

/// Use case that builds a file-level dependency graph across one or more repositories.
///
/// For each repository the use case:
/// 1. Loads a symbol-name → file-path map from the vector store (chunks).
/// 2. Loads all symbol references from the call graph.
/// 3. For every reference whose callee symbol can be resolved to a file,
///    emits a weighted directed edge: `caller_file → callee_file`.
/// 4. Returns a [`FileGraph`] containing the edges, node set, and repo metadata.
pub struct FileRelationshipUseCase {
    call_graph: Arc<CallGraphUseCase>,
    vector_repo: Arc<dyn VectorRepository>,
    metadata_repo: Arc<dyn MetadataRepository>,
}

impl FileRelationshipUseCase {
    pub fn new(
        call_graph: Arc<CallGraphUseCase>,
        vector_repo: Arc<dyn VectorRepository>,
        metadata_repo: Arc<dyn MetadataRepository>,
    ) -> Self {
        Self {
            call_graph,
            vector_repo,
            metadata_repo,
        }
    }

    /// Build the file dependency graph.
    ///
    /// * `repository_ids` — when `Some`, restricts the graph to the listed
    ///   repositories; `None` means "all indexed repositories".
    /// * `min_weight` — minimum number of distinct symbol references required
    ///   before an edge is included in the result.
    /// * `include_cross_repo` — when `true`, edges whose endpoints belong to
    ///   different repositories are also included.
    pub async fn build_graph(
        &self,
        repository_ids: Option<&[String]>,
        min_weight: usize,
        include_cross_repo: bool,
    ) -> Result<FileGraph, DomainError> {
        // ── 1. Resolve which repositories to analyse ───────────────────────
        let all_repos = self
            .metadata_repo
            .list()
            .await
            .map_err(|e| DomainError::storage(format!("Failed to list repositories: {e}")))?;

        let target_repos: Vec<_> = if let Some(ids) = repository_ids {
            all_repos
                .into_iter()
                .filter(|r| ids.contains(&r.id().to_string()))
                .collect()
        } else {
            all_repos
        };

        if target_repos.is_empty() {
            return Ok(FileGraph {
                repositories: HashMap::new(),
                files: HashSet::new(),
                edges: vec![],
            });
        }

        // ── 2. Build symbol_name → (file_path, repo_id) lookup ────────────
        // Prefer the first entry when the same symbol name appears in multiple
        // files (e.g. a trait defined in a module and re-exported).
        let mut symbol_map: HashMap<String, (String, String)> = HashMap::new();
        for repo in &target_repos {
            let entries = self
                .vector_repo
                .get_symbol_to_file_map(repo.id())
                .await?;
            for (sym, file) in entries {
                symbol_map.entry(sym).or_insert((file, repo.id().to_string()));
            }
        }

        // ── 3. Aggregate symbol references into file-level edges ──────────
        // Key: (from_file, from_repo_id, to_file, to_repo_id)
        // Value: (weight, reference_kinds)
        let mut edge_map: HashMap<(String, String, String, String), (usize, HashSet<String>)> =
            HashMap::new();

        for repo in &target_repos {
            let refs = self.call_graph.find_by_repository(repo.id()).await?;

            for sr in refs {
                let callee = sr.callee_symbol();
                let Some((to_file, to_repo)) = symbol_map.get(callee) else {
                    continue;
                };

                let from_file = sr.caller_file_path().to_string();
                let from_repo = sr.repository_id().to_string();

                // Skip self-loops (same file on both ends).
                if from_file == *to_file {
                    continue;
                }

                // Optionally skip cross-repo edges.
                if !include_cross_repo && from_repo != *to_repo {
                    continue;
                }

                let key = (
                    from_file,
                    from_repo,
                    to_file.clone(),
                    to_repo.clone(),
                );
                let entry = edge_map.entry(key).or_insert((0, HashSet::new()));
                entry.0 += 1;
                entry.1.insert(sr.reference_kind().as_str().to_string());
            }
        }

        // ── 4. Materialise into FileEdge + FileGraph ──────────────────────
        let mut edges: Vec<FileEdge> = edge_map
            .into_iter()
            .filter(|(_, (w, _))| *w >= min_weight)
            .map(
                |((from_file, from_repo_id, to_file, to_repo_id), (weight, kinds))| FileEdge {
                    from_file,
                    from_repo_id,
                    to_file,
                    to_repo_id,
                    weight,
                    reference_kinds: {
                        let mut v: Vec<String> = kinds.into_iter().collect();
                        v.sort();
                        v
                    },
                },
            )
            .collect();

        // Deterministic order: heaviest edges first, then alphabetical.
        edges.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.from_file.cmp(&b.from_file))
                .then(a.to_file.cmp(&b.to_file))
        });

        let mut files: HashSet<String> = HashSet::new();
        for e in &edges {
            files.insert(e.from_file.clone());
            files.insert(e.to_file.clone());
        }

        let repositories: HashMap<String, FileGraphRepo> = target_repos
            .into_iter()
            .map(|r| {
                (
                    r.id().to_string(),
                    FileGraphRepo {
                        id: r.id().to_string(),
                        name: r.name().to_string(),
                        path: r.path().to_string(),
                    },
                )
            })
            .collect();

        Ok(FileGraph {
            repositories,
            files,
            edges,
        })
    }
}
