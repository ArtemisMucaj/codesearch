use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use tracing::{debug, info, warn};

/// TypeScript project files scip-typescript will honour if present. When any of
/// these exist we leave configuration to the repository and don't synthesise our
/// own — clobbering a real project's compiler options would be wrong.
const TS_PROJECT_FILES: &[&str] = &["tsconfig.json", "jsconfig.json"];

/// Minimal `tsconfig.json` synthesised for repositories that ship none. Without
/// `allowJs`, TypeScript (and therefore scip-typescript) matches zero files in a
/// vanilla JavaScript project and aborts with "no files got indexed". The broad
/// `include` glob plus `allowJs` makes it pick up `.js`/`.jsx`/`.mjs`/`.cjs`
/// sources; `checkJs`/`noEmit` keep it from type-checking or emitting anything.
const SYNTHESISED_TSCONFIG: &str = r#"{
  "compilerOptions": {
    "allowJs": true,
    "checkJs": false,
    "noEmit": true,
    "moduleResolution": "node",
    "target": "esnext",
    "module": "esnext"
  },
  "include": ["**/*.js", "**/*.jsx", "**/*.mjs", "**/*.cjs"],
  "exclude": ["node_modules"]
}
"#;

/// Guard for a synthesised `tsconfig.json` written into a repo that ships none,
/// so we never leave stray config behind (the old `--infer-tsconfig` path left
/// an empty `{}` file that also failed to index).
///
/// Creation is atomic (`create_new`) and cleanup is content-verified — the file
/// is removed only while it still holds exactly what we wrote. This makes the
/// guard safe against a real `tsconfig.json` appearing concurrently: we never
/// overwrite one on the way in, nor delete a foreign one on the way out.
struct TempTsconfig {
    path: PathBuf,
}

impl TempTsconfig {
    /// Atomically create a temporary `tsconfig.json` at `repo_path`, unless one
    /// of the [`TS_PROJECT_FILES`] already exists (in which case we leave the
    /// repo's own config untouched and return `Ok(None)`).
    ///
    /// `create_new` closes the check-then-write race: if a `tsconfig.json`
    /// appears between the existence check and the create, the create fails with
    /// `AlreadyExists` and we back off rather than clobber it.
    async fn create_if_absent(repo_path: &Path) -> Result<Option<Self>> {
        for f in TS_PROJECT_FILES {
            if tokio::fs::try_exists(repo_path.join(f))
                .await
                .unwrap_or(false)
            {
                debug!("Repository already has a TS/JS project config; not synthesising one");
                return Ok(None);
            }
        }

        let path = repo_path.join("tsconfig.json");
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                file.write_all(SYNTHESISED_TSCONFIG.as_bytes())
                    .await
                    .map_err(|e| {
                        anyhow!("failed to write temporary tsconfig at {:?}: {}", path, e)
                    })?;
                debug!("Synthesised temporary tsconfig at {:?}", path);
                Ok(Some(Self { path }))
            }
            // A real config raced in after the check — leave it to the repo.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                debug!("A tsconfig.json appeared concurrently; not synthesising one");
                Ok(None)
            }
            Err(e) => Err(anyhow!(
                "failed to create temporary tsconfig at {:?}: {}",
                path,
                e
            )),
        }
    }

    /// Remove the synthesised file, but only if it still contains exactly what we
    /// wrote — never delete a config that was replaced out from under us.
    async fn cleanup(self) {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) if contents == SYNTHESISED_TSCONFIG => {
                if let Err(e) = tokio::fs::remove_file(&self.path).await {
                    warn!(
                        "failed to remove temporary tsconfig at {:?}: {}",
                        self.path, e
                    );
                }
            }
            Ok(_) => debug!(
                "temporary tsconfig at {:?} was modified; leaving it in place",
                self.path
            ),
            Err(e) => warn!(
                "failed to read temporary tsconfig at {:?} for cleanup: {}",
                self.path, e
            ),
        }
    }
}

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
            IndexerKind::Php => vec!["-o".to_string(), output_path.to_string_lossy().to_string()],
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
            IndexerKind::Php => "Install it from: https://github.com/ArtemisMucaj/scip-php",
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

        let output_path = repo_path.join(match kind {
            IndexerKind::TypeScript => "index-typescript.scip",
            IndexerKind::Php => "index-php.scip",
        });

        // scip-typescript needs a tsconfig/jsconfig with `allowJs` to index a
        // vanilla JavaScript repo. Synthesise a throwaway one when the repo
        // ships none, and remove it once indexing finishes. PHP needs no such
        // config.
        let tsconfig_guard = match kind {
            IndexerKind::TypeScript => TempTsconfig::create_if_absent(repo_path).await?,
            IndexerKind::Php => None,
        };

        info!("Running {} in {:?}", kind.display_name(), repo_path);

        let result = tokio::process::Command::new(kind.binary())
            .args(kind.args(&output_path))
            .current_dir(repo_path)
            .output()
            .await;

        // Clean up the synthesised tsconfig before returning down any path.
        if let Some(guard) = tsconfig_guard {
            guard.cleanup().await;
        }

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
                let stdout = String::from_utf8_lossy(&output.stdout);
                let output_msg = match (stdout.trim(), stderr.trim()) {
                    ("", "") => "(no output)".to_string(),
                    ("", e) => e.to_string(),
                    (o, "") => o.to_string(),
                    (o, e) => format!("stdout: {o}\nstderr: {e}"),
                };
                Err(anyhow!(
                    "{} failed (exit {:?}):\n{}",
                    kind.display_name(),
                    output.status.code(),
                    output_msg
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn synthesises_tsconfig_when_absent_and_cleans_it_up() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tsconfig.json");

        let guard = TempTsconfig::create_if_absent(dir.path())
            .await
            .unwrap()
            .expect("should synthesise a config when none exists");
        // The synthesised file holds exactly our template.
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(written, SYNTHESISED_TSCONFIG);

        guard.cleanup().await;
        // Content-verified cleanup removed the file it created.
        assert!(!tokio::fs::try_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn leaves_existing_config_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tsconfig.json");
        tokio::fs::write(&path, "{ \"compilerOptions\": { \"strict\": true } }")
            .await
            .unwrap();

        // A real config exists → no guard, and the file is not overwritten.
        let guard = TempTsconfig::create_if_absent(dir.path()).await.unwrap();
        assert!(guard.is_none());
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(after.contains("strict"));
    }

    #[tokio::test]
    async fn does_not_synthesise_when_jsconfig_present() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("jsconfig.json"), "{}")
            .await
            .unwrap();

        let guard = TempTsconfig::create_if_absent(dir.path()).await.unwrap();
        assert!(
            guard.is_none(),
            "a jsconfig.json must also suppress synthesis"
        );
    }

    #[tokio::test]
    async fn cleanup_preserves_a_replaced_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tsconfig.json");

        let guard = TempTsconfig::create_if_absent(dir.path())
            .await
            .unwrap()
            .unwrap();
        // Something replaces the file out from under us (e.g. the repo added its
        // own config mid-run). Cleanup must not delete a foreign file.
        tokio::fs::write(&path, "{ \"compilerOptions\": { \"strict\": true } }")
            .await
            .unwrap();

        guard.cleanup().await;
        assert!(tokio::fs::try_exists(&path).await.unwrap());
        let preserved = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(preserved.contains("strict"));
    }
}
