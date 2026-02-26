use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, info, warn};

use crate::application::{
    CallGraphUseCase, EmbeddingService, FileHashRepository, MetadataRepository, ParserService,
    VectorRepository,
};
use crate::domain::{
    compute_file_hash, DomainError, FileHash, Language, LanguageStats, Repository, SymbolReference,
    VectorStore,
};

/// Port trait for the SCIP indexing phase.
///
/// Implementations live in the connector layer (e.g. `ScipPhaseRunner`) so
/// that the application layer stays free of external tool dependencies.
///
/// The method is **fallible**: when a SCIP indexer binary is found on `PATH`
/// but its execution fails, the implementation returns `Err` so that the
/// error surfaces to the user.  Returns `Ok(empty map)` when the repository
/// contains no files of a SCIP-supported language.
#[async_trait::async_trait]
pub trait ScipPhase: Send + Sync {
    /// Run SCIP indexers for `repo_path` and return a map of
    /// `relative_file_path → pre-extracted SymbolReferences`.
    ///
    /// Returns `Err` when an available indexer binary failed to produce an
    /// index.  Returns `Ok(empty)` when the repository has no SCIP-language
    /// files (no indexer binary is invoked in that case).
    async fn run(
        &self,
        repo_path: &Path,
        repo_id: &str,
        has_js_ts: bool,
        has_php: bool,
    ) -> Result<HashMap<String, Vec<SymbolReference>>, DomainError>;
}

pub struct IndexRepositoryUseCase {
    repository_repo: Arc<dyn MetadataRepository>,
    vector_repo: Arc<dyn VectorRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
    parser_service: Arc<dyn ParserService>,
    embedding_service: Arc<dyn EmbeddingService>,
    /// Optional SCIP phase.  When present, JS/TS/PHP files use SCIP-derived
    /// symbol references instead of (or as a fallback from) tree-sitter.
    scip_phase: Option<Arc<dyn ScipPhase>>,
}

impl IndexRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn MetadataRepository>,
        vector_repo: Arc<dyn VectorRepository>,
        file_hash_repo: Arc<dyn FileHashRepository>,
        call_graph_use_case: Arc<CallGraphUseCase>,
        parser_service: Arc<dyn ParserService>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            repository_repo,
            vector_repo,
            file_hash_repo,
            call_graph_use_case,
            parser_service,
            embedding_service,
            scip_phase: None,
        }
    }

    /// Attach an optional SCIP phase runner.
    pub fn with_scip_phase(mut self, scip_phase: Arc<dyn ScipPhase>) -> Self {
        self.scip_phase = Some(scip_phase);
        self
    }

    /// Returns `true` for languages handled by SCIP indexers (JS/TS and PHP).
    fn is_scip_language(language: Language) -> bool {
        matches!(
            language,
            Language::JavaScript | Language::TypeScript | Language::Php
        )
    }

    /// Delegate to the injected [`ScipPhase`], or return an empty map when
    /// no SCIP phase is configured (e.g. in tests).
    ///
    /// Propagates errors from the phase so that a failed indexer aborts
    /// indexing immediately.
    async fn run_scip_phase(
        &self,
        absolute_path: &Path,
        repo_id: &str,
        has_js_ts: bool,
        has_php: bool,
    ) -> Result<HashMap<String, Vec<SymbolReference>>, DomainError> {
        match &self.scip_phase {
            Some(phase) => phase.run(absolute_path, repo_id, has_js_ts, has_php).await,
            None => Ok(HashMap::new()),
        }
    }

    pub async fn execute(
        &self,
        path: &str,
        name: Option<&str>,
        store: VectorStore,
        namespace: Option<String>,
        force: bool,
    ) -> Result<Repository, DomainError> {
        let path = Path::new(path);
        let absolute_path = path
            .canonicalize()
            .map_err(|e| DomainError::InvalidInput(format!("Invalid path: {}", e)))?;

        let path_str = absolute_path.to_string_lossy().to_string();

        // Check if repository already exists
        let existing = self.repository_repo.find_by_path(&path_str).await?;

        if force {
            // Force re-index: delete everything and start fresh
            if let Some(ref existing) = existing {
                info!(
                    "Force re-indexing repository (deleting existing data): {}",
                    path_str
                );
                self.vector_repo.delete_by_repository(existing.id()).await?;
                self.file_hash_repo
                    .delete_by_repository(existing.id())
                    .await?;
                self.call_graph_use_case
                    .delete_by_repository(existing.id())
                    .await?;
                self.repository_repo.delete(existing.id()).await?;
            }
            return self
                .index(&absolute_path, &path_str, name, store, namespace)
                .await;
        }

        match existing {
            Some(repository) => {
                // Incremental indexing
                info!("Incremental indexing repository: {}", path_str);
                self.incremental_index(&absolute_path, &repository).await
            }
            None => {
                // First-time indexing
                self.index(&absolute_path, &path_str, name, store, namespace)
                    .await
            }
        }
    }

    async fn index(
        &self,
        absolute_path: &Path,
        path_str: &str,
        name: Option<&str>,
        store: VectorStore,
        namespace: Option<String>,
    ) -> Result<Repository, DomainError> {
        let repo_name = name.map(String::from).unwrap_or_else(|| {
            absolute_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        let repository =
            Repository::new_with_storage(repo_name.clone(), path_str.to_string(), store, namespace);
        self.repository_repo.save(&repository).await?;

        info!("Indexing repository: {} at {}", repo_name, path_str);

        let start_time = Instant::now();

        // First pass: collect all files to process
        let files_to_process: Vec<_> = WalkBuilder::new(absolute_path)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .filter(|entry| {
                let language = Language::from_path(entry.path());
                language != Language::Unknown && self.parser_service.supports_language(language)
            })
            .collect();

        let total_files = files_to_process.len() as u64;
        info!("Found {} files to index", total_files);

        // Phase 0 — pre-scan JS/TS files to build a map of file → exported symbol names.
        // This map is used during reference extraction to resolve `const X = require('./path')`
        // to the actual exported symbol rather than the local binding name.
        let pre_scan_paths: Vec<String> = files_to_process
            .iter()
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(absolute_path)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let exports_by_file = self
            .run_export_pre_scan(absolute_path, &pre_scan_paths)
            .await;

        // Phase 1 — SCIP: run any available SCIP indexers (scip-typescript / scip-php)
        // and pre-load their symbol references.  Files covered by SCIP skip tree-sitter
        // call graph extraction in the loop below, giving compiler-accurate resolution.
        let has_js_ts = files_to_process.iter().any(|e| {
            matches!(
                Language::from_path(e.path()),
                Language::JavaScript | Language::TypeScript
            )
        });
        let has_php = files_to_process
            .iter()
            .any(|e| Language::from_path(e.path()) == Language::Php);
        let scip_refs = self
            .run_scip_phase(absolute_path, repository.id(), has_js_ts, has_php)
            .await?;

        let progress_bar = ProgressBar::new(total_files);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("  {spinner:.green} {bar:40.green/dim} {pos}/{len} {msg:.dim}")
                .expect("Invalid progress bar template")
                .progress_chars("━━─"),
        );

        let mut file_count = 0u64;
        let mut chunk_count = 0u64;
        let mut reference_count = 0u64;
        let mut file_hashes = Vec::new();
        let mut language_stats: HashMap<String, LanguageStats> = HashMap::new();

        for entry in files_to_process {
            let entry_path = entry.path();
            let language = Language::from_path(entry_path);

            let relative_path = entry_path
                .strip_prefix(absolute_path)
                .unwrap_or(entry_path)
                .to_string_lossy()
                .to_string();

            progress_bar.set_message(relative_path.clone());
            debug!("Processing file: {}", relative_path);

            let content = match tokio::fs::read_to_string(entry_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read file {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            // Compute and store file hash
            let content_hash = compute_file_hash(&content);
            file_hashes.push(FileHash::new(
                relative_path.clone(),
                content_hash,
                repository.id().to_string(),
            ));

            let chunks = match self
                .parser_service
                .parse_file(&content, &relative_path, language, repository.id())
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to parse file {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            if !chunks.is_empty() {
                let embeddings = match self.embedding_service.embed_chunks(&chunks).await {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Failed to generate embeddings for {}: {}", relative_path, e);
                        progress_bar.inc(1);
                        continue;
                    }
                };
                self.vector_repo.save_batch(&chunks, &embeddings).await?;
            }

            let refs_count = if Self::is_scip_language(language) {
                // SCIP languages: call graph comes from the SCIP index only.
                // Tree-sitter is not used here — it only handles chunking above.
                if let Some(scip_file_refs) = scip_refs.get(&relative_path) {
                    debug!(
                        "Using {} SCIP references for {}",
                        scip_file_refs.len(),
                        relative_path
                    );
                    self.call_graph_use_case
                        .save_references(scip_file_refs)
                        .await
                        .map_err(|e| DomainError::internal(format!("{:#}", e)))?
                } else {
                    0
                }
            } else {
                // Non-SCIP languages: call graph via tree-sitter.
                self.call_graph_use_case
                    .extract_and_save(
                        &content,
                        &relative_path,
                        language,
                        repository.id(),
                        &exports_by_file,
                    )
                    .await
                    .map_err(|e| DomainError::internal(format!("{:#}", e)))?
            };
            reference_count += refs_count;

            file_count += 1;
            chunk_count += chunks.len() as u64;

            // Track language statistics
            let lang_key = language.as_str().to_string();
            let stats = language_stats.entry(lang_key).or_default();
            stats.file_count += 1;
            stats.chunk_count += chunks.len() as u64;

            debug!(
                "Indexed {} chunks, {} references from {}",
                chunks.len(),
                refs_count,
                relative_path
            );
            progress_bar.inc(1);
        }

        progress_bar.finish_and_clear();

        // Save all file hashes
        self.file_hash_repo.save_batch(&file_hashes).await?;

        self.repository_repo
            .update_stats(repository.id(), chunk_count, file_count)
            .await?;

        self.repository_repo
            .update_languages(repository.id(), language_stats)
            .await?;

        let duration = start_time.elapsed();
        info!(
            "Indexing complete: {} files, {} chunks, {} references in {:.2}s",
            file_count,
            chunk_count,
            reference_count,
            duration.as_secs_f64()
        );

        self.vector_repo.flush().await?;

        self.repository_repo
            .find_by_id(repository.id())
            .await?
            .ok_or_else(|| DomainError::internal("Repository not found after indexing"))
    }

    /// Run the JS/TS export pre-scan and return the resulting map.
    /// Shared by the initial and incremental index paths.
    async fn run_export_pre_scan(
        &self,
        absolute_path: &Path,
        pre_scan_paths: &[String],
    ) -> HashMap<String, Vec<String>> {
        let exports = self
            .call_graph_use_case
            .build_export_index(absolute_path, pre_scan_paths)
            .await;
        debug!(
            "Export pre-scan complete: {} JS/TS files with detectable exports",
            exports.len()
        );
        exports
    }

    async fn incremental_index(
        &self,
        absolute_path: &Path,
        repository: &Repository,
    ) -> Result<Repository, DomainError> {
        let start_time = Instant::now();

        // Load existing file hashes
        let existing_hashes = self
            .file_hash_repo
            .find_by_repository(repository.id())
            .await?;
        let existing_hash_map: HashMap<String, String> = existing_hashes
            .into_iter()
            .map(|h| (h.file_path().to_string(), h.content_hash().to_string()))
            .collect();

        // Collect current files
        let mut current_files: HashMap<String, String> = HashMap::new();
        let walker = WalkBuilder::new(absolute_path)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("Error walking directory: {}", e);
                    continue;
                }
            };
            let entry_path = entry.path();

            if !entry_path.is_file() {
                continue;
            }

            let language = Language::from_path(entry_path);
            if language == Language::Unknown || !self.parser_service.supports_language(language) {
                continue;
            }

            let relative_path = entry_path
                .strip_prefix(absolute_path)
                .unwrap_or(entry_path)
                .to_string_lossy()
                .to_string();

            let content = match tokio::fs::read_to_string(entry_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read file {}: {}", relative_path, e);
                    continue;
                }
            };

            let content_hash = compute_file_hash(&content);
            current_files.insert(relative_path, content_hash);
        }

        // Detect changes
        let current_paths: HashSet<&String> = current_files.keys().collect();
        let existing_paths: HashSet<&String> = existing_hash_map.keys().collect();

        let added: Vec<&String> = current_paths.difference(&existing_paths).copied().collect();
        let deleted: Vec<&String> = existing_paths.difference(&current_paths).copied().collect();
        let modified: Vec<&String> = current_paths
            .intersection(&existing_paths)
            .filter(|path| current_files.get(**path) != existing_hash_map.get(**path))
            .copied()
            .collect();
        let unchanged_count = current_paths.len() - added.len() - modified.len();

        info!(
            "Detected changes: {} added, {} modified, {} deleted, {} unchanged",
            added.len(),
            modified.len(),
            deleted.len(),
            unchanged_count
        );

        // Track total chunks deleted
        let mut deleted_chunk_count = 0u64;

        // Process deleted files (remove chunks and references)
        for path in &deleted {
            debug!("Removing deleted file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
            // Also delete symbol references for this file
            self.call_graph_use_case
                .delete_by_file(repository.id(), path)
                .await?;
        }
        if !deleted.is_empty() {
            let deleted_paths: Vec<String> = deleted.iter().map(|s| s.to_string()).collect();
            self.file_hash_repo
                .delete_by_paths(repository.id(), &deleted_paths)
                .await?;
        }

        // Process modified files (delete old chunks and references, then re-index)
        for path in &modified {
            debug!("Re-indexing modified file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
            // Also delete symbol references for this file
            self.call_graph_use_case
                .delete_by_file(repository.id(), path)
                .await?;
        }

        // Phase 0 — pre-scan ALL JS/TS files in the repo (including unchanged ones) to build
        // an exports map for require() resolution.  We need the full set because an added or
        // modified file may import from an unchanged file.
        let pre_scan_paths: Vec<String> = current_files.keys().cloned().collect();
        let exports_by_file = self
            .run_export_pre_scan(absolute_path, &pre_scan_paths)
            .await;

        // Phase 1 — SCIP: same as the full-index path.  We re-run the indexer even for
        // incremental updates because a JS/TS file that imports a modified module may have
        // stale cross-file resolution in the old tree-sitter references.
        let has_js_ts = current_files
            .keys()
            .any(|p| matches!(Language::from_path(Path::new(p)), Language::JavaScript | Language::TypeScript));
        let has_php = current_files
            .keys()
            .any(|p| Language::from_path(Path::new(p)) == Language::Php);
        let scip_refs = self
            .run_scip_phase(absolute_path, repository.id(), has_js_ts, has_php)
            .await?;

        // Process added and modified files
        let files_to_process: Vec<&String> = added.iter().chain(modified.iter()).copied().collect();
        let total_to_process = files_to_process.len() as u64;

        let progress_bar = ProgressBar::new(total_to_process);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("  {spinner:.green} {bar:40.green/dim} {pos}/{len} {msg:.dim}")
                .expect("Invalid progress bar template")
                .progress_chars("━━─"),
        );

        let mut new_file_hashes = Vec::new();
        let mut processed_count = 0u64;
        let mut new_chunk_count = 0u64;
        let mut new_reference_count = 0u64;
        let mut language_stats: HashMap<String, LanguageStats> = HashMap::new();

        for relative_path in files_to_process {
            progress_bar.set_message(relative_path.clone());

            let entry_path = absolute_path.join(relative_path);
            let language = Language::from_path(&entry_path);

            let content = match tokio::fs::read_to_string(&entry_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read file {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            let content_hash = current_files
                .get(relative_path)
                .cloned()
                .unwrap_or_else(|| compute_file_hash(&content));

            let chunks = match self
                .parser_service
                .parse_file(&content, relative_path, language, repository.id())
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to parse file {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            if !chunks.is_empty() {
                let embeddings = match self.embedding_service.embed_chunks(&chunks).await {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Failed to generate embeddings for {}: {}", relative_path, e);
                        progress_bar.inc(1);
                        continue;
                    }
                };
                self.vector_repo.save_batch(&chunks, &embeddings).await?;
            }

            let refs_count = if Self::is_scip_language(language) {
                // SCIP languages: call graph comes from the SCIP index only.
                // Tree-sitter is not used here — it only handles chunking above.
                if let Some(scip_file_refs) = scip_refs.get(relative_path) {
                    debug!(
                        "Using {} SCIP references for {}",
                        scip_file_refs.len(),
                        relative_path
                    );
                    self.call_graph_use_case
                        .save_references(scip_file_refs)
                        .await
                        .map_err(|e| DomainError::internal(format!("{:#}", e)))?
                } else {
                    0
                }
            } else {
                // Non-SCIP languages: call graph via tree-sitter.
                self.call_graph_use_case
                    .extract_and_save(
                        &content,
                        relative_path,
                        language,
                        repository.id(),
                        &exports_by_file,
                    )
                    .await
                    .map_err(|e| DomainError::internal(format!("{:#}", e)))?
            };
            new_reference_count += refs_count;

            // Only add file hash after successful indexing
            new_file_hashes.push(FileHash::new(
                relative_path.clone(),
                content_hash,
                repository.id().to_string(),
            ));

            processed_count += 1;
            new_chunk_count += chunks.len() as u64;

            // Track language statistics for new/modified files
            let lang_key = language.as_str().to_string();
            let stats = language_stats.entry(lang_key).or_default();
            stats.file_count += 1;
            stats.chunk_count += chunks.len() as u64;

            debug!(
                "Indexed {} chunks, {} references from {}",
                chunks.len(),
                refs_count,
                relative_path
            );
            progress_bar.inc(1);
        }

        progress_bar.finish_and_clear();

        // Track language statistics for unchanged files
        // We need to count them by language based on their file extensions
        for path in current_paths.intersection(&existing_paths) {
            if !modified.contains(path) {
                let entry_path = absolute_path.join(*path);
                let language = Language::from_path(&entry_path);
                if language != Language::Unknown {
                    let lang_key = language.as_str().to_string();
                    let stats = language_stats.entry(lang_key).or_default();
                    stats.file_count += 1;
                    // Note: We don't have chunk counts for unchanged files without querying DB
                    // For simplicity, we'll just track file counts; chunk counts for unchanged
                    // files would require an additional query
                }
            }
        }

        // Save new file hashes
        if !new_file_hashes.is_empty() {
            self.file_hash_repo.save_batch(&new_file_hashes).await?;
        }

        // Calculate total stats
        let total_file_count = unchanged_count as u64 + processed_count;
        let previous_chunk_count = repository.chunk_count();
        let total_chunk_count = previous_chunk_count - deleted_chunk_count + new_chunk_count;

        self.repository_repo
            .update_stats(repository.id(), total_chunk_count, total_file_count)
            .await?;

        self.repository_repo
            .update_languages(repository.id(), language_stats)
            .await?;

        let duration = start_time.elapsed();
        info!(
            "Incremental indexing complete: processed {} files ({} new chunks, {} references) in {:.2}s",
            processed_count,
            new_chunk_count,
            new_reference_count,
            duration.as_secs_f64()
        );

        self.vector_repo.flush().await?;

        self.repository_repo
            .find_by_id(repository.id())
            .await?
            .ok_or_else(|| DomainError::internal("Repository not found after indexing"))
    }
}
