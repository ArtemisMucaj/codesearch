use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, info, warn};

use crate::application::{
    CallGraphUseCase, EmbeddingService, FileHashRepository, MetadataRepository, ParserService,
    VectorRepository,
};
use crate::domain::{
    compute_file_hash, DomainError, Embedding, FileHash, Language, LanguageStats, Repository,
    SymbolReference, VectorStore,
};

/// Default number of concurrent `parse_only` calls during the parse phase.
const DEFAULT_PARSE_CONCURRENCY: usize = 4;

/// Number of chunks accumulated across files before a single `embed_chunks`
/// call is issued.  Larger values amortise per-call overhead and produce
/// better-utilised inference batches.
const CROSS_FILE_EMBED_BATCH: usize = 256;

/// Port trait for the SCIP indexing phase.
///
/// Implementations live in the connector layer (e.g. `ScipRunner`) so
/// that the application layer stays free of external tool dependencies.
///
/// The method is **fallible**: when a SCIP indexer binary is found on `PATH`
/// but its execution fails, the implementation returns `Err` so that the
/// error surfaces to the user.  Returns `Ok(empty map)` when the repository
/// contains no files of a SCIP-supported language.
#[async_trait::async_trait]
pub trait Scip: Send + Sync {
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
    /// Optional SCIP indexer.  When present, JS/TS/PHP files use SCIP-derived
    /// symbol references instead of (or as a fallback from) tree-sitter.
    scip: Option<Arc<dyn Scip>>,
    /// Maximum number of concurrent `parse_only` calls.
    embed_concurrency: usize,
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
            scip: None,
            embed_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }

    /// Attach an optional SCIP indexer.
    pub fn with_scip(mut self, scip: Arc<dyn Scip>) -> Self {
        self.scip = Some(scip);
        self
    }

    /// Set the maximum number of concurrent parse tasks.
    pub fn with_embed_concurrency(mut self, n: usize) -> Self {
        self.embed_concurrency = n.max(1);
        self
    }

    /// Delegate to the injected [`Scip`] indexer, or return an empty map when
    /// none is configured (e.g. in tests).
    ///
    /// Propagates errors so that a failed indexer aborts indexing immediately.
    async fn run_scip(
        &self,
        absolute_path: &Path,
        repo_id: &str,
        has_js_ts: bool,
        has_php: bool,
    ) -> Result<HashMap<String, Vec<SymbolReference>>, DomainError> {
        match &self.scip {
            Some(scip) => scip.run(absolute_path, repo_id, has_js_ts, has_php).await,
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

    /// Embed and write one accumulated batch of pre-parsed files.
    ///
    /// All chunks across every file in `batch` are sent to the embedding
    /// service in a single call so that the model can process a large,
    /// efficient batch.  Returns `(file_count, chunk_count, reference_count)`.
    ///
    /// On success `batch` is cleared.  On embedding failure the batch is also
    /// cleared (files are skipped with a warning) so that the caller can
    /// continue with the next batch.
    async fn flush_embed_batch(
        &self,
        batch: &mut Vec<ParseOnlyResult>,
        repository: &Repository,
        scip_refs: &HashMap<String, Vec<SymbolReference>>,
        language_stats: &mut HashMap<String, LanguageStats>,
    ) -> Result<(u64, u64, u64), DomainError> {
        if batch.is_empty() {
            return Ok((0, 0, 0));
        }

        // Drain chunks out of each file result into a flat Vec for a single
        // embed_chunks call, recording how many chunks each file contributed.
        let mut flat_chunks: Vec<crate::domain::CodeChunk> = Vec::new();
        let mut per_file_chunk_count: Vec<usize> = Vec::with_capacity(batch.len());

        for result in batch.iter_mut() {
            per_file_chunk_count.push(result.chunks.len());
            flat_chunks.append(&mut result.chunks);
        }

        // Single embedding call for the entire batch.
        let all_embeddings: Vec<Embedding> = if flat_chunks.is_empty() {
            vec![]
        } else {
            match self.embedding_service.embed_chunks(&flat_chunks).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "Failed to embed batch of {} chunks across {} files: {}",
                        flat_chunks.len(),
                        batch.len(),
                        e
                    );
                    batch.clear();
                    return Ok((0, 0, 0));
                }
            }
        };

        // Distribute the flat embeddings back to their originating files and
        // write each file's chunks + embeddings to the vector store.
        let mut chunk_offset = 0usize;
        let mut emb_offset = 0usize;
        let mut file_count = 0u64;
        let mut chunk_count = 0u64;
        let mut ref_count = 0u64;

        for (i, result) in batch.iter().enumerate() {
            let n = per_file_chunk_count[i];
            let file_chunks = &flat_chunks[chunk_offset..chunk_offset + n];
            let file_embeddings = &all_embeddings[emb_offset..emb_offset + n];
            chunk_offset += n;
            emb_offset += n;

            // Delete any pre-existing chunks for this file before inserting
            // new ones.  On a clean first-time index this is a no-op; on a
            // crash-resume it removes stale chunks from an interrupted run.
            self.vector_repo
                .delete_by_file_path(repository.id(), &result.relative_path)
                .await?;

            if !file_chunks.is_empty() {
                self.vector_repo
                    .save_batch(file_chunks, file_embeddings)
                    .await?;
            }

            let refs_count =
                if let Some(scip_file_refs) = scip_refs.get(&result.relative_path) {
                    debug!(
                        "Using {} SCIP references for {}",
                        scip_file_refs.len(),
                        result.relative_path
                    );
                    // Delete stale call-graph rows before inserting new ones.
                    self.call_graph_use_case
                        .delete_by_file(repository.id(), &result.relative_path)
                        .await?;
                    self.call_graph_use_case
                        .save_references(scip_file_refs)
                        .await
                        .map_err(|e| DomainError::internal(format!("{:#}", e)))?
                } else {
                    0
                };
            ref_count += refs_count;

            // Write the file hash immediately after its chunks so that a crash
            // between two files never leaves the DB in a state where chunks
            // exist without a corresponding hash record.
            self.file_hash_repo
                .save_batch(&[FileHash::new(
                    result.relative_path.clone(),
                    result.content_hash.clone(),
                    repository.id().to_string(),
                )])
                .await?;

            file_count += 1;
            chunk_count += n as u64;

            let lang_key = result.language.as_str().to_string();
            let stats = language_stats.entry(lang_key).or_default();
            stats.file_count += 1;
            stats.chunk_count += n as u64;

            debug!(
                "Indexed {} chunks, {} references from {}",
                n, refs_count, result.relative_path
            );
        }

        batch.clear();
        Ok((file_count, chunk_count, ref_count))
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
        let files_to_process: Vec<PathBuf> = WalkBuilder::new(absolute_path)
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
            .map(|entry| entry.path().to_path_buf())
            .collect();

        let total_files = files_to_process.len() as u64;
        info!("Found {} files to index", total_files);

        let has_js_ts = files_to_process.iter().any(|p| {
            matches!(
                Language::from_path(p),
                Language::JavaScript | Language::TypeScript
            )
        });
        let has_php = files_to_process
            .iter()
            .any(|p| Language::from_path(p) == Language::Php);
        let scip_refs = self
            .run_scip(absolute_path, repository.id(), has_js_ts, has_php)
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
        let mut language_stats: HashMap<String, LanguageStats> = HashMap::new();

        // Phase 1 (concurrent): read → parse
        // Phase 2 (batched):    embed across multiple files in a single call
        // Phase 3 (sequential): write to DuckDB + update stats
        //
        // Parsing concurrency is set higher than the legacy embed_concurrency
        // because the parse phase is cheaper (I/O + tree-sitter) than
        // embedding.  4× gives a good parse pipeline depth; minimum 8 ensures
        // responsiveness on small repos.
        let parse_concurrency = (self.embed_concurrency * 4).max(8);

        let repo_id = repository.id().to_string();
        let abs_path = absolute_path.to_path_buf();
        let parser_service = self.parser_service.clone();

        let mut stream = futures_util::stream::iter(files_to_process)
            .map(move |entry_path| {
                let parser_service = parser_service.clone();
                let abs_path = abs_path.clone();
                let repo_id = repo_id.clone();
                async move { parse_only(entry_path, &abs_path, &repo_id, &*parser_service).await }
            })
            .buffer_unordered(parse_concurrency);

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;

        while let Some(maybe_result) = stream.next().await {
            progress_bar.inc(1);
            if let Some(result) = maybe_result {
                progress_bar.set_message(result.relative_path.clone());
                pending_chunk_count += result.chunks.len();
                pending.push(result);

                if pending_chunk_count >= CROSS_FILE_EMBED_BATCH {
                    let (fc, cc, rc) = self
                        .flush_embed_batch(
                            &mut pending,
                            &repository,
                            &scip_refs,
                            &mut language_stats,
                        )
                        .await?;
                    file_count += fc;
                    chunk_count += cc;
                    reference_count += rc;
                    pending_chunk_count = 0;
                }
            }
        }

        // Flush any remaining files that didn't fill a full batch.
        if !pending.is_empty() {
            let (fc, cc, rc) = self
                .flush_embed_batch(
                    &mut pending,
                    &repository,
                    &scip_refs,
                    &mut language_stats,
                )
                .await?;
            file_count += fc;
            chunk_count += cc;
            reference_count += rc;
        }

        progress_bar.finish_and_clear();

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
            self.call_graph_use_case
                .delete_by_file(repository.id(), path)
                .await?;
        }

        // SCIP: same as the full-index path.
        let has_js_ts = current_files
            .keys()
            .any(|p| matches!(Language::from_path(Path::new(p)), Language::JavaScript | Language::TypeScript));
        let has_php = current_files
            .keys()
            .any(|p| Language::from_path(Path::new(p)) == Language::Php);
        let scip_refs = self
            .run_scip(absolute_path, repository.id(), has_js_ts, has_php)
            .await?;

        // Convert relative path strings to PathBufs for the stream
        let files_to_process: Vec<PathBuf> = added
            .iter()
            .chain(modified.iter())
            .map(|p| absolute_path.join(p))
            .collect();
        let total_to_process = files_to_process.len() as u64;

        let progress_bar = ProgressBar::new(total_to_process);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("  {spinner:.green} {bar:40.green/dim} {pos}/{len} {msg:.dim}")
                .expect("Invalid progress bar template")
                .progress_chars("━━─"),
        );

        let mut new_processed_paths: HashSet<String> = HashSet::new();
        let mut processed_count = 0u64;
        let mut new_chunk_count = 0u64;
        let mut new_reference_count = 0u64;
        let mut language_stats: HashMap<String, LanguageStats> = HashMap::new();

        // Precompute hashes from the walk above so we don't re-read the files
        // just for the hash in the sequential phase.
        let current_files_snapshot = current_files.clone();

        let parse_concurrency = (self.embed_concurrency * 4).max(8);

        let repo_id = repository.id().to_string();
        let abs_path = absolute_path.to_path_buf();
        let parser_service = self.parser_service.clone();

        let mut stream = futures_util::stream::iter(files_to_process)
            .map(move |entry_path| {
                let parser_service = parser_service.clone();
                let abs_path = abs_path.clone();
                let repo_id = repo_id.clone();
                async move { parse_only(entry_path, &abs_path, &repo_id, &*parser_service).await }
            })
            .buffer_unordered(parse_concurrency);

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;

        while let Some(maybe_result) = stream.next().await {
            progress_bar.inc(1);
            if let Some(mut result) = maybe_result {
                progress_bar.set_message(result.relative_path.clone());

                // Prefer the hash computed during the directory walk (already
                // in memory) to avoid a potential TOCTOU difference.
                if let Some(walk_hash) = current_files_snapshot.get(&result.relative_path) {
                    result.content_hash = walk_hash.clone();
                }

                pending_chunk_count += result.chunks.len();
                pending.push(result);

                if pending_chunk_count >= CROSS_FILE_EMBED_BATCH {
                    // Capture paths before flush_embed_batch clears the batch.
                    let paths: Vec<String> =
                        pending.iter().map(|r| r.relative_path.clone()).collect();
                    let (fc, cc, rc) = self
                        .flush_embed_batch(
                            &mut pending,
                            repository,
                            &scip_refs,
                            &mut language_stats,
                        )
                        .await?;
                    new_processed_paths.extend(paths);
                    new_chunk_count += cc;
                    new_reference_count += rc;
                    processed_count += fc;
                    pending_chunk_count = 0;
                }
            }
        }

        // Flush remaining
        if !pending.is_empty() {
            let paths: Vec<String> = pending.iter().map(|r| r.relative_path.clone()).collect();
            let (fc, cc, rc) = self
                .flush_embed_batch(
                    &mut pending,
                    repository,
                    &scip_refs,
                    &mut language_stats,
                )
                .await?;
            new_processed_paths.extend(paths);
            new_chunk_count += cc;
            new_reference_count += rc;
            processed_count += fc;
        }

        progress_bar.finish_and_clear();

        // SCIP references for unchanged files.
        for (relative_path, file_refs) in &scip_refs {
            if new_processed_paths.contains(relative_path) {
                continue;
            }
            self.call_graph_use_case
                .delete_by_file(repository.id(), relative_path)
                .await?;
            new_reference_count += self
                .call_graph_use_case
                .save_references(file_refs)
                .await
                .map_err(|e| DomainError::internal(format!("{:#}", e)))?;
        }

        // Track language statistics for unchanged files
        for path in current_paths.intersection(&existing_paths) {
            if !modified.contains(path) {
                let entry_path = absolute_path.join(*path);
                let language = Language::from_path(&entry_path);
                if language != Language::Unknown {
                    let lang_key = language.as_str().to_string();
                    let stats = language_stats.entry(lang_key).or_default();
                    stats.file_count += 1;
                }
            }
        }

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

/// Result of parsing a single file, before embedding.
struct ParseOnlyResult {
    relative_path: String,
    content_hash: String,
    language: Language,
    chunks: Vec<crate::domain::CodeChunk>,
}

/// Read and parse a single file without generating embeddings.
///
/// Returns `None` when the file should be skipped (read/parse failure);
/// warnings are emitted in that case.
async fn parse_only(
    entry_path: PathBuf,
    absolute_path: &Path,
    repo_id: &str,
    parser_service: &dyn ParserService,
) -> Option<ParseOnlyResult> {
    let language = Language::from_path(&entry_path);
    let relative_path = entry_path
        .strip_prefix(absolute_path)
        .unwrap_or(&entry_path)
        .to_string_lossy()
        .to_string();

    let content = match tokio::fs::read_to_string(&entry_path).await {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read file {}: {}", relative_path, e);
            return None;
        }
    };

    let content_hash = compute_file_hash(&content);

    let chunks = match parser_service
        .parse_file(&content, &relative_path, language, repo_id)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse file {}: {}", relative_path, e);
            return None;
        }
    };

    Some(ParseOnlyResult {
        relative_path,
        content_hash,
        language,
        chunks,
    })
}
