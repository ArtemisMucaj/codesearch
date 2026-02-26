use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use crate::application::use_cases::Scip;
use crate::domain::{DomainError, SymbolReference};

use super::{run_applicable_indexers, ScipImporter};

/// Concrete implementation of [`Scip`] that shells out to
/// `scip-typescript` and/or `scip-php`, parses the resulting index files,
/// and returns symbol references keyed by relative file path.
///
/// When neither indexer is installed the method returns `Ok(empty)` so the
/// caller uses tree-sitter as before.  When an indexer **is** installed but
/// fails, `Err` is returned — the failure is never silently swallowed.
pub struct ScipRunner;

#[async_trait::async_trait]
impl Scip for ScipRunner {
    async fn run(
        &self,
        repo_path: &Path,
        repo_id: &str,
        has_js_ts: bool,
        has_php: bool,
    ) -> Result<HashMap<String, Vec<SymbolReference>>, DomainError> {
        let index_files = run_applicable_indexers(repo_path, has_js_ts, has_php)
            .await
            .map_err(|e| DomainError::internal(format!("SCIP indexer failed: {:#}", e)))?;

        if index_files.is_empty() {
            return Ok(HashMap::new());
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
                    // Import failures (corrupt index, I/O error) are also hard errors
                    // since the indexer was available and we expected a valid index.
                    return Err(DomainError::internal(format!(
                        "SCIP import failed for {:?}: {:#}",
                        scip_path, e
                    )));
                }
            }
        }

        Ok(combined)
    }
}
