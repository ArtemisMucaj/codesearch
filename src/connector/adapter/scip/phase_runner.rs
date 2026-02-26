use std::collections::HashMap;
use std::path::Path;

use tracing::{info, warn};

use crate::application::use_cases::ScipPhase;
use crate::domain::SymbolReference;

use super::{run_applicable_indexers, ScipImporter};

/// Concrete implementation of [`ScipPhase`] that shells out to
/// `scip-typescript` and/or `scip-php`, parses the resulting index files,
/// and returns symbol references keyed by relative file path.
///
/// When neither indexer is available the method returns an empty map so the
/// caller falls back to tree-sitter extraction transparently.
pub struct ScipPhaseRunner;

#[async_trait::async_trait]
impl ScipPhase for ScipPhaseRunner {
    async fn run(
        &self,
        repo_path: &Path,
        repo_id: &str,
        has_js_ts: bool,
        has_php: bool,
    ) -> HashMap<String, Vec<SymbolReference>> {
        let index_files = run_applicable_indexers(repo_path, has_js_ts, has_php).await;

        if index_files.is_empty() {
            return HashMap::new();
        }

        let mut combined: HashMap<String, Vec<SymbolReference>> = HashMap::new();

        for (_kind, scip_path) in index_files {
            match ScipImporter::import(&scip_path, repo_id).await {
                Ok(by_file) => {
                    let file_count = by_file.len();
                    let ref_count: usize =
                        by_file.values().map(|v: &Vec<SymbolReference>| v.len()).sum();
                    info!(
                        "SCIP import: {} files, {} references from {:?}",
                        file_count, ref_count, scip_path
                    );
                    for (file, refs) in by_file {
                        combined.entry(file).or_default().extend(refs);
                    }
                }
                Err(e) => {
                    warn!("SCIP import failed for {:?}: {:#}", scip_path, e);
                }
            }
        }

        combined
    }
}
