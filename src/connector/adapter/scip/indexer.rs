use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use tracing::{debug, info};

/// Shells out to a SCIP indexer binary and returns the path to the generated
/// `index.scip` file.
///
/// Supported indexers:
/// - `scip-typescript` — handles JavaScript and TypeScript
/// - `scip-php`        — handles PHP
///
/// Both indexers are **required** when the repository contains the matching
/// language files.  If the binary is not on `PATH`, indexing fails with a
/// clear install hint rather than silently degrading to tree-sitter.
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

    /// Arguments passed to the binary.
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

    /// Human-readable install instructions shown when the binary is missing.
    fn install_hint(&self) -> &'static str {
        match self {
            IndexerKind::TypeScript => {
                "Install it with: npm install -g @sourcegraph/scip-typescript"
            }
            IndexerKind::Php => "Install it from: https://github.com/davidrjenni/scip-php",
        }
    }
}

impl ScipIndexer {
    /// Run `kind` against `repo_path` and return the path to the generated
    /// index file.
    ///
    /// Returns `Err` in every failure case:
    /// - binary not on `PATH` → actionable install hint
    /// - non-zero exit code   → stderr forwarded to the user
    /// - index file missing after a successful exit → bug report hint
    pub async fn run(repo_path: &Path, kind: IndexerKind) -> Result<PathBuf> {
        if !Self::binary_available(kind).await {
            return Err(anyhow!(
                "'{}' was not found on PATH.\n  {}",
                kind.binary(),
                kind.install_hint(),
            ));
        }

        let output_path = repo_path.join("index.scip");
        info!("Running {} in {:?}", kind.display_name(), repo_path);

        let result = tokio::process::Command::new(kind.binary())
            .args(kind.args(&output_path))
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
                    Ok(output_path)
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

    /// Try to load an already-generated `index.scip` from the repo root.
    ///
    /// When present this file takes precedence over running any indexer,
    /// making it the recommended path for CI environments.
    pub fn find_existing(repo_path: &Path) -> Option<PathBuf> {
        let candidate = repo_path.join("index.scip");
        if candidate.exists() {
            debug!("Found existing index.scip at {:?}", candidate);
            Some(candidate)
        } else {
            None
        }
    }

    /// Returns `true` if the binary is present and responds to `--version`.
    async fn binary_available(kind: IndexerKind) -> bool {
        tokio::process::Command::new(kind.binary())
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Run all required SCIP indexers for a repository.
///
/// If a pre-existing `index.scip` is found in the repo root it is used as-is
/// and no indexer binary is invoked.  Otherwise the appropriate indexer(s) are
/// run and an error is returned if any of them are missing or fail.
pub async fn run_applicable_indexers(
    repo_path: &Path,
    has_js_ts: bool,
    has_php: bool,
) -> Result<Vec<(IndexerKind, PathBuf)>> {
    // Pre-existing index takes precedence; the importer determines languages
    // per document so a single file covers both JS/TS and PHP.
    if let Some(existing) = ScipIndexer::find_existing(repo_path) {
        info!(
            "Using pre-existing index.scip at {:?} (skipping indexer invocation)",
            existing
        );
        return Ok(vec![(IndexerKind::TypeScript, existing)]);
    }

    let mut results = Vec::new();

    if has_js_ts {
        let path = ScipIndexer::run(repo_path, IndexerKind::TypeScript).await?;
        results.push((IndexerKind::TypeScript, path));
    }

    if has_php {
        let path = ScipIndexer::run(repo_path, IndexerKind::Php).await?;
        results.push((IndexerKind::Php, path));
    }

    Ok(results)
}
