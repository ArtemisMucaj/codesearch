use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
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
/// call is issued.  Larger values amortise per-call overhead, produce
/// better-utilised inference batches, and reduce DuckDB transaction count.
const CROSS_FILE_EMBED_BATCH: usize = 512;

/// Return type of [`do_flush`]: file count, chunk count, ref count, and per-
/// language stats accumulated for the flushed batch.
type FlushStats = (u64, u64, u64, HashMap<String, LanguageStats>);

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
    parse_concurrency: usize,
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
            parse_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }

    /// Attach an optional SCIP indexer.
    pub fn with_scip(mut self, scip: Arc<dyn Scip>) -> Self {
        self.scip = Some(scip);
        self
    }

    /// Set the maximum number of concurrent parse tasks.
    pub fn with_parse_concurrency(mut self, n: usize) -> Self {
        self.parse_concurrency = n.max(1);
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

    /// Spawn a [`do_flush`] task, cloning the necessary [`Arc`] handles.
    ///
    /// Returns a [`JoinHandle`] that resolves to the flush statistics.  The
    /// caller is responsible for awaiting the handle before accessing the
    /// accumulated counters.
    fn spawn_flush(
        &self,
        batch: Vec<ParseOnlyResult>,
        repository_id: String,
        scip_refs: &Arc<HashMap<String, Vec<SymbolReference>>>,
    ) -> JoinHandle<Result<FlushStats, DomainError>> {
        tokio::spawn(do_flush(
            batch,
            repository_id,
            Arc::clone(scip_refs),
            Arc::clone(&self.embedding_service),
            Arc::clone(&self.vector_repo),
            Arc::clone(&self.file_hash_repo),
            Arc::clone(&self.call_graph_use_case),
        ))
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
        let scip_refs = Arc::new(
            self.run_scip(absolute_path, repository.id(), has_js_ts, has_php)
                .await?,
        );

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

        // Parsing and embedding/writing are fully pipelined:
        //
        //  • The parse stream runs in its own tokio task (spawn_parse_stream),
        //    so its futures advance on the thread pool independently of this
        //    task.  Results are sent through an mpsc channel.
        //
        //  • Each full batch is handed to a spawned flush task (do_flush).
        //    At most one flush is in-flight at a time (double-buffering).
        //
        //  • When this task awaits a flush handle, the parse task keeps
        //    running and the channel buffers incoming results.  On a large
        //    repo the parser stays comfortably ahead of the embedder.
        let parse_concurrency = (self.parse_concurrency * 4).max(8);

        let mut parse_rx = spawn_parse_stream(
            files_to_process,
            absolute_path.to_path_buf(),
            repository.id().to_string(),
            self.parser_service.clone(),
            parse_concurrency,
        );

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;
        let mut pending_flush: Option<JoinHandle<Result<FlushStats, DomainError>>> = None;

        while let Some(maybe_result) = parse_rx.recv().await {
            progress_bar.inc(1);
            if let Some(result) = maybe_result {
                progress_bar.set_message(result.relative_path.clone());
                pending_chunk_count += result.chunks.len();
                pending.push(result);

                if pending_chunk_count >= CROSS_FILE_EMBED_BATCH {
                    // Collect the previous flush result before spawning a new
                    // one.  The previous flush ran concurrently with stream
                    // consumption above, so it is often already finished.
                    if let Some(task) = pending_flush.take() {
                        let stats = task.await.map_err(|e| {
                            DomainError::internal(format!("Flush task panicked: {e}"))
                        })??;
                        merge_stats(
                            stats,
                            &mut file_count,
                            &mut chunk_count,
                            &mut reference_count,
                            &mut language_stats,
                        );
                    }
                    let batch = std::mem::take(&mut pending);
                    pending_chunk_count = 0;
                    pending_flush =
                        Some(self.spawn_flush(batch, repository.id().to_string(), &scip_refs));
                }
            }
        }

        // Collect the last spawned flush (if any).
        if let Some(task) = pending_flush.take() {
            let stats = task
                .await
                .map_err(|e| DomainError::internal(format!("Flush task panicked: {e}")))?
                ?;
            merge_stats(
                stats,
                &mut file_count,
                &mut chunk_count,
                &mut reference_count,
                &mut language_stats,
            );
        }

        // Flush any remaining files that didn't fill a complete batch.
        if !pending.is_empty() {
            let stats = do_flush(
                std::mem::take(&mut pending),
                repository.id().to_string(),
                Arc::clone(&scip_refs),
                Arc::clone(&self.embedding_service),
                Arc::clone(&self.vector_repo),
                Arc::clone(&self.file_hash_repo),
                Arc::clone(&self.call_graph_use_case),
            )
            .await?;
            merge_stats(
                stats,
                &mut file_count,
                &mut chunk_count,
                &mut reference_count,
                &mut language_stats,
            );
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
        let has_js_ts = current_files.keys().any(|p| {
            matches!(
                Language::from_path(Path::new(p)),
                Language::JavaScript | Language::TypeScript
            )
        });
        let has_php = current_files
            .keys()
            .any(|p| Language::from_path(Path::new(p)) == Language::Php);
        let scip_refs = Arc::new(
            self.run_scip(absolute_path, repository.id(), has_js_ts, has_php)
                .await?,
        );

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

        let parse_concurrency = (self.parse_concurrency * 4).max(8);

        let mut parse_rx = spawn_parse_stream(
            files_to_process,
            absolute_path.to_path_buf(),
            repository.id().to_string(),
            self.parser_service.clone(),
            parse_concurrency,
        );

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;
        let mut pending_flush: Option<JoinHandle<Result<FlushStats, DomainError>>> = None;

        while let Some(maybe_result) = parse_rx.recv().await {
            progress_bar.inc(1);
            if let Some(mut result) = maybe_result {
                progress_bar.set_message(result.relative_path.clone());

                // Only fall back to the walk hash when parse_only did not
                // produce one (empty); the parse_only hash is derived from
                // the content that was actually read and parsed, so it is
                // more accurate and should be preferred.
                if result.content_hash.is_empty() {
                    if let Some(walk_hash) = current_files_snapshot.get(&result.relative_path) {
                        result.content_hash = walk_hash.clone();
                    }
                }

                pending_chunk_count += result.chunks.len();
                // Record the path as changed at the moment we decide to
                // process it so it is present in new_processed_paths even
                // if the subsequent flush fails non-fatally or is deferred.
                new_processed_paths.insert(result.relative_path.clone());
                pending.push(result);

                if pending_chunk_count >= CROSS_FILE_EMBED_BATCH {
                    if let Some(task) = pending_flush.take() {
                        let stats = task.await.map_err(|e| {
                            DomainError::internal(format!("Flush task panicked: {e}"))
                        })??;
                        let (fc, cc, rc, ld) = stats;
                        processed_count += fc;
                        new_chunk_count += cc;
                        new_reference_count += rc;
                        for (k, v) in ld {
                            let s = language_stats.entry(k).or_default();
                            s.file_count += v.file_count;
                            s.chunk_count += v.chunk_count;
                        }
                    }
                    let batch = std::mem::take(&mut pending);
                    pending_chunk_count = 0;
                    pending_flush =
                        Some(self.spawn_flush(batch, repository.id().to_string(), &scip_refs));
                }
            }
        }

        // Collect the last spawned flush (if any).
        if let Some(task) = pending_flush.take() {
            let (fc, cc, rc, ld) = task
                .await
                .map_err(|e| DomainError::internal(format!("Flush task panicked: {e}")))?
                ?;
            processed_count += fc;
            new_chunk_count += cc;
            new_reference_count += rc;
            for (k, v) in ld {
                let s = language_stats.entry(k).or_default();
                s.file_count += v.file_count;
                s.chunk_count += v.chunk_count;
            }
        }

        // Flush remaining
        if !pending.is_empty() {
            let (fc, cc, rc, ld) = do_flush(
                std::mem::take(&mut pending),
                repository.id().to_string(),
                Arc::clone(&scip_refs),
                Arc::clone(&self.embedding_service),
                Arc::clone(&self.vector_repo),
                Arc::clone(&self.file_hash_repo),
                Arc::clone(&self.call_graph_use_case),
            )
            .await?;
            processed_count += fc;
            new_chunk_count += cc;
            new_reference_count += rc;
            for (k, v) in ld {
                let s = language_stats.entry(k).or_default();
                s.file_count += v.file_count;
                s.chunk_count += v.chunk_count;
            }
        }

        progress_bar.finish_and_clear();

        // SCIP references for unchanged files.
        for (relative_path, file_refs) in scip_refs.iter() {
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

/// Accumulate the stats returned by a single [`do_flush`] call into the
/// running totals held by the caller.
fn merge_stats(
    (fc, cc, rc, lang_delta): FlushStats,
    file_count: &mut u64,
    chunk_count: &mut u64,
    ref_count: &mut u64,
    language_stats: &mut HashMap<String, LanguageStats>,
) {
    *file_count += fc;
    *chunk_count += cc;
    *ref_count += rc;
    for (k, v) in lang_delta {
        let s = language_stats.entry(k).or_default();
        s.file_count += v.file_count;
        s.chunk_count += v.chunk_count;
    }
}

/// Spawn the parse stream as an independent tokio task and return the
/// receiving end of an [`mpsc`] channel.
///
/// Running the stream inside its own task means that `parse_only` futures
/// advance on the tokio thread pool even when the calling task is blocked
/// awaiting a flush handle.  The channel acts as a bounded buffer so the
/// parser stays ahead of the embedder without unbounded memory growth.
fn spawn_parse_stream(
    files: Vec<PathBuf>,
    abs_path: PathBuf,
    repo_id: String,
    parser_service: Arc<dyn ParserService>,
    concurrency: usize,
) -> mpsc::Receiver<Option<ParseOnlyResult>> {
    // Buffer enough results to absorb a full flush cycle without stalling.
    let (tx, rx) = mpsc::channel(concurrency * 8);
    tokio::spawn(async move {
        let mut stream = futures_util::stream::iter(files)
            .map(move |entry_path| {
                let parser_service = parser_service.clone();
                let abs_path = abs_path.clone();
                let repo_id = repo_id.clone();
                async move { parse_only(entry_path, &abs_path, &repo_id, &*parser_service).await }
            })
            .buffer_unordered(concurrency);

        while let Some(result) = stream.next().await {
            if tx.send(result).await.is_err() {
                break; // consumer dropped the receiver — indexing was aborted
            }
        }
    });
    rx
}

/// Embed and persist one accumulated batch of pre-parsed files.
///
/// This is a free function (not a method) so it can be passed directly to
/// [`tokio::spawn`], enabling the parse stream to continue making forward
/// progress while embedding and DB writes happen on a separate task.
///
/// All service references are passed as `Arc` clones; they are cheap to copy
/// and do not require any additional locking beyond what each service already
/// provides internally.
///
/// Returns `(file_count, chunk_count, ref_count, language_stats)`.
async fn do_flush(
    batch: Vec<ParseOnlyResult>,
    repository_id: String,
    scip_refs: Arc<HashMap<String, Vec<SymbolReference>>>,
    embedding_service: Arc<dyn EmbeddingService>,
    vector_repo: Arc<dyn VectorRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
) -> Result<FlushStats, DomainError> {
    if batch.is_empty() {
        return Ok((0, 0, 0, HashMap::new()));
    }

    // ── Phase 1: flatten chunks for a single embed_chunks call ───────────────
    let mut flat_chunks: Vec<crate::domain::CodeChunk> = Vec::new();
    let mut per_file_chunk_count: Vec<usize> = Vec::with_capacity(batch.len());

    for result in &batch {
        per_file_chunk_count.push(result.chunks.len());
    }
    // Move chunks out of the batch without cloning.
    let batch: Vec<ParseOnlyResult> = batch
        .into_iter()
        .map(|mut r| {
            flat_chunks.append(&mut r.chunks);
            r
        })
        .collect();

    // ── Phase 2: embed all chunks in one call ─────────────────────────────────
    // On failure fall back to per-file calls so a single bad file cannot
    // discard the whole batch.
    let per_file_embeddings: Vec<Option<Vec<Embedding>>> = if flat_chunks.is_empty() {
        per_file_chunk_count.iter().map(|_| Some(vec![])).collect()
    } else {
        match embedding_service.embed_chunks(&flat_chunks).await {
            Ok(all) => {
                let mut drain = all.into_iter();
                per_file_chunk_count
                    .iter()
                    .map(|&n| Some(drain.by_ref().take(n).collect::<Vec<_>>()))
                    .collect()
            }
            Err(e) => {
                warn!(
                    "Failed to embed batch of {} chunks across {} files, \
                     retrying per-file: {}",
                    flat_chunks.len(),
                    batch.len(),
                    e
                );
                let mut offset = 0;
                let mut results = Vec::with_capacity(batch.len());
                for (i, &n) in per_file_chunk_count.iter().enumerate() {
                    let file_chunks = &flat_chunks[offset..offset + n];
                    offset += n;
                    if file_chunks.is_empty() {
                        results.push(Some(vec![]));
                        continue;
                    }
                    match embedding_service.embed_chunks(file_chunks).await {
                        Ok(embeds) => results.push(Some(embeds)),
                        Err(file_err) => {
                            warn!(
                                "Per-file embedding failed for {}: {}",
                                batch[i].relative_path, file_err
                            );
                            results.push(None);
                        }
                    }
                }
                results
            }
        }
    };

    // ── Phase 3: batch-delete stale data, then batch-write new data ──────────
    //
    // One delete transaction covers all files.
    // One save_batch transaction covers all valid chunks + embeddings.
    // One save_batch transaction covers all file hashes.

    let all_paths: Vec<&str> = batch.iter().map(|r| r.relative_path.as_str()).collect();
    vector_repo
        .delete_by_file_paths(&repository_id, &all_paths)
        .await?;

    let mut valid_chunks: Vec<crate::domain::CodeChunk> = Vec::new();
    let mut valid_embeddings: Vec<Embedding> = Vec::new();
    let mut file_hashes: Vec<FileHash> = Vec::new();
    let mut language_stats: HashMap<String, LanguageStats> = HashMap::new();

    let mut chunk_offset = 0usize;
    let mut file_count = 0u64;
    let mut chunk_count = 0u64;

    for (i, result) in batch.iter().enumerate() {
        let n = per_file_chunk_count[i];
        let file_chunks = &flat_chunks[chunk_offset..chunk_offset + n];
        chunk_offset += n;

        let file_embeddings = match per_file_embeddings[i].as_deref() {
            Some(e) => e,
            None => continue, // embedding failed for this file; skip it
        };

        valid_chunks.extend_from_slice(file_chunks);
        valid_embeddings.extend_from_slice(file_embeddings);

        file_hashes.push(FileHash::new(
            result.relative_path.clone(),
            result.content_hash.clone(),
            repository_id.clone(),
        ));

        file_count += 1;
        chunk_count += n as u64;

        let lang_key = result.language.as_str().to_string();
        let stats = language_stats.entry(lang_key).or_default();
        stats.file_count += 1;
        stats.chunk_count += n as u64;
    }

    if !valid_chunks.is_empty() {
        vector_repo
            .save_batch(&valid_chunks, &valid_embeddings)
            .await?;
    }

    // Call-graph refs: per-file, only present for SCIP-indexed files.
    let mut ref_count = 0u64;
    for result in &batch {
        let refs_count = if let Some(scip_file_refs) = scip_refs.get(&result.relative_path) {
            debug!(
                "Using {} SCIP references for {}",
                scip_file_refs.len(),
                result.relative_path
            );
            call_graph_use_case
                .delete_by_file(&repository_id, &result.relative_path)
                .await?;
            call_graph_use_case
                .save_references(scip_file_refs)
                .await
                .map_err(|e| DomainError::internal(format!("{:#}", e)))?
        } else {
            0
        };
        ref_count += refs_count;
    }

    if !file_hashes.is_empty() {
        file_hash_repo.save_batch(&file_hashes).await?;
    }

    debug!(
        "Flushed batch: {} files, {} chunks, {} references",
        file_count, chunk_count, ref_count
    );

    Ok((file_count, chunk_count, ref_count, language_stats))
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
