use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use tracing::{debug, info};

/// Shells out to an installed SCIP indexer binary and returns the path to the
/// generated `index.scip` file.
///
/// Supported indexers (tried in order when the repo contains matching files):
/// - `scip-typescript` — handles JavaScript and TypeScript
/// - `scip-php`        — handles PHP
///
/// Each indexer is **mandatory when available**: if the binary is found on
/// `PATH` but the run fails, an error is returned and indexing is aborted for
/// that language rather than silently falling back to tree-sitter.  If the
/// binary is not installed at all, the language is skipped gracefully.
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

    /// Arguments passed to the binary once it has been confirmed to exist.
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
    /// Try to run the given SCIP indexer against `repo_path`.
    ///
    /// Returns:
    /// - `Ok(Some(path))` — binary found and ran successfully; `path` points to
    ///   the generated index file.
    /// - `Ok(None)` — binary not installed; caller should skip gracefully.
    /// - `Err(e)` — binary **was** found on `PATH` but execution failed.
    ///   Callers must surface this error; silent fallback is not acceptable.
    pub async fn try_run(repo_path: &Path, kind: IndexerKind) -> Result<Option<PathBuf>> {
        if !Self::binary_available(kind).await {
            debug!(
                "SCIP indexer '{}' not found on PATH, skipping",
                kind.binary()
            );
            return Ok(None);
        }

        let output_path = repo_path.join("index.scip");
        info!("Running {} in {:?}", kind.display_name(), repo_path);

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
                    Ok(Some(output_path))
                } else {
                    Err(anyhow!(
                        "{} exited successfully but {:?} was not created",
                        kind.display_name(),
                        output_path
                    ))
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(anyhow!(
                    "{} failed (exit {:?}): {}",
                    kind.display_name(),
                    output.status.code(),
                    stderr.trim()
                ))
            }
            Err(e) => Err(anyhow!("failed to spawn {}: {}", kind.binary(), e)),
        }
    }

    /// Try to load an already-generated `index.scip` file from the repo root.
    ///
    /// This allows users to pre-generate the index (e.g. in CI) and have
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

    /// Returns `true` if the indexer binary is present and responds to
    /// `--version`.
    async fn binary_available(kind: IndexerKind) -> bool {
        tokio::process::Command::new(kind.binary())
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Run all applicable SCIP indexers for a repository.
///
/// Returns `(IndexerKind, PathBuf)` pairs for every indexer that produced an
/// index file.  Returns an error if any **available** indexer fails — "available"
/// meaning the binary was found on `PATH` but the run itself did not succeed.
/// Indexers whose binary is simply not installed are skipped silently.
pub async fn run_applicable_indexers(
    repo_path: &Path,
    has_js_ts: bool,
    has_php: bool,
) -> Result<Vec<(IndexerKind, PathBuf)>> {
    // A pre-existing index.scip takes precedence — no indexer invocation needed.
    if let Some(existing) = ScipIndexer::find_existing(repo_path) {
        info!(
            "Using pre-existing index.scip at {:?} (skipping indexer invocation)",
            existing
        );
        return Ok(vec![(IndexerKind::TypeScript, existing)]);
    }

    let mut results = Vec::new();

    if has_js_ts {
        match ScipIndexer::try_run(repo_path, IndexerKind::TypeScript).await? {
            Some(path) => results.push((IndexerKind::TypeScript, path)),
            None => {} // binary not installed — skip
        }
    }

    if has_php {
        match ScipIndexer::try_run(repo_path, IndexerKind::Php).await? {
            Some(path) => results.push((IndexerKind::Php, path)),
            None => {} // binary not installed — skip
        }
    }

    Ok(results)
}
