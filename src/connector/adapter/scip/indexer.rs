use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

/// Shells out to an installed SCIP indexer binary and returns the path to the
/// generated `index.scip` file.
///
/// Supported indexers (tried in order when the repo contains matching files):
/// - `scip-typescript` — handles JavaScript and TypeScript
/// - `scip-php`        — handles PHP
///
/// Each indexer is optional: if the binary is not on `PATH` the language is
/// silently skipped and tree-sitter extraction is used as the fallback.
pub struct ScipIndexer;

/// Which SCIP indexer to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexerKind {
    /// `scip-typescript` — covers `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`
    TypeScript,
    /// `scip-php` — covers `.php`
    Php,
}

impl IndexerKind {
    /// The binary name looked up on `PATH`.
    fn binary(&self) -> &'static str {
        match self {
            IndexerKind::TypeScript => "scip-typescript",
            IndexerKind::Php => "scip-php",
        }
    }

    /// Arguments passed to the binary (after checking it exists).
    fn args(&self, output_path: &Path) -> Vec<String> {
        match self {
            IndexerKind::TypeScript => vec![
                "index".to_string(),
                "--output".to_string(),
                output_path.to_string_lossy().to_string(),
            ],
            IndexerKind::Php => vec![
                "--output".to_string(),
                output_path.to_string_lossy().to_string(),
            ],
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            IndexerKind::TypeScript => "scip-typescript",
            IndexerKind::Php => "scip-php",
        }
    }
}

impl ScipIndexer {
    /// Run the given SCIP indexer against `repo_path` and return the path to
    /// the generated index file, or `None` if the binary is unavailable or the
    /// run fails.
    pub async fn try_run(repo_path: &Path, kind: IndexerKind) -> Option<PathBuf> {
        if !Self::binary_available(kind).await {
            debug!(
                "SCIP indexer '{}' not found on PATH, skipping",
                kind.binary()
            );
            return None;
        }

        let output_path = repo_path.join("index.scip");
        info!(
            "Running {} in {:?}",
            kind.display_name(),
            repo_path
        );

        let args = kind.args(&output_path);
        let result = tokio::process::Command::new(kind.binary())
            .args(&args)
            .current_dir(repo_path)
            .output()
            .await;

        match result {
            Ok(output) if output.status.success() => {
                if output_path.exists() {
                    info!(
                        "{} succeeded, index at {:?}",
                        kind.display_name(),
                        output_path
                    );
                    Some(output_path)
                } else {
                    warn!(
                        "{} exited successfully but {:?} was not created",
                        kind.display_name(),
                        output_path
                    );
                    None
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    "{} failed (exit {:?}): {}",
                    kind.display_name(),
                    output.status.code(),
                    stderr.trim()
                );
                None
            }
            Err(e) => {
                warn!("Failed to spawn {}: {}", kind.binary(), e);
                None
            }
        }
    }

    /// Try to load an already-generated `index.scip` file from the repo root.
    ///
    /// This allows users to pre-generate the index (e.g., in CI) and have
    /// codesearch consume it without needing the indexer installed locally.
    pub fn find_existing(repo_path: &Path) -> Option<PathBuf> {
        let candidate = repo_path.join("index.scip");
        if candidate.exists() {
            debug!("Found existing index.scip at {:?}", candidate);
            Some(candidate)
        } else {
            None
        }
    }

    /// Returns `true` if the indexer binary is available on `PATH`.
    async fn binary_available(kind: IndexerKind) -> bool {
        tokio::process::Command::new(kind.binary())
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Run all applicable SCIP indexers for a repository and return the paths of
/// any successfully generated index files, tagged by indexer kind.
///
/// Tries `scip-typescript` if the repo contains JS/TS files, and `scip-php`
/// if it contains PHP files.
pub async fn run_applicable_indexers(
    repo_path: &Path,
    has_js_ts: bool,
    has_php: bool,
) -> Vec<(IndexerKind, PathBuf)> {
    // First check for a pre-existing index.scip (covers both JS/TS and PHP if
    // generated by a multi-language indexer).
    if let Some(existing) = ScipIndexer::find_existing(repo_path) {
        info!(
            "Using pre-existing index.scip at {:?} (skipping indexer invocation)",
            existing
        );
        // Tag as TypeScript since that's the primary generator; importer
        // determines the actual language per document.
        return vec![(IndexerKind::TypeScript, existing)];
    }

    let mut results = Vec::new();

    if has_js_ts {
        if let Some(path) = ScipIndexer::try_run(repo_path, IndexerKind::TypeScript).await {
            results.push((IndexerKind::TypeScript, path));
        }
    }

    if has_php {
        if let Some(path) = ScipIndexer::try_run(repo_path, IndexerKind::Php).await {
            results.push((IndexerKind::Php, path));
        }
    }

    results
}
