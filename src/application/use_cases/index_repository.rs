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

use crate::application::git_remote::detect_remote;
use crate::application::{
    is_messaging_package, AnalysisRepository, CallGraphUseCase, ChannelEndpointRepository,
    ChannelExtractor, ChannelResolver, EmbeddingService, FileHashRepository, MetadataRepository,
    ParserService, ResolveChannelsUseCase, VectorRepository,
};
use crate::domain::{
    compute_file_hash, namespace_scope_id, ChannelEndpoint, DomainError, Embedding, EndpointSource,
    FileHash, Language, LanguageStats, Repository, SymbolReference, VectorStore,
};

/// Default number of concurrent `parse_only` calls during the parse phase.
const DEFAULT_PARSE_CONCURRENCY: usize = 4;

/// Number of chunks accumulated across files before a single `embed_chunks`
/// call is issued.  Smaller values produce more frequent flushes and smoother
/// progress bar movement; the embedder receives well-sized batches either way
/// since `embed_chunks` processes all chunks in one call regardless of count.
const CROSS_FILE_EMBED_BATCH: usize = 128;

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
    /// Optional channel-endpoint extraction (cross-service linking).  When
    /// both are present, every parsed file also runs the channel extractor
    /// and the endpoints are persisted alongside chunks and references.
    channel_extractor: Option<Arc<dyn ChannelExtractor>>,
    channel_endpoint_repo: Option<Arc<dyn ChannelEndpointRepository>>,
    /// Optional cross-file channel resolution (library confirmation + config
    /// value resolution). Runs once after extraction, using the SCIP refs and
    /// the repo's config modules.
    channel_resolver: Option<Arc<dyn ChannelResolver>>,
    /// Optional store of derived analyses (Leiden clusters, symbol communities,
    /// execution features).  Stored analyses are invalidated whenever indexing
    /// changes the call graph they were computed from.
    analysis_repo: Option<Arc<dyn AnalysisRepository>>,
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
            channel_extractor: None,
            channel_endpoint_repo: None,
            channel_resolver: None,
            analysis_repo: None,
            parse_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }

    /// Attach an optional SCIP indexer.
    pub fn with_scip(mut self, scip: Arc<dyn Scip>) -> Self {
        self.scip = Some(scip);
        self
    }

    /// Attach channel-endpoint extraction (cross-service linking).
    pub fn with_channel_extraction(
        mut self,
        extractor: Arc<dyn ChannelExtractor>,
        repository: Arc<dyn ChannelEndpointRepository>,
    ) -> Self {
        self.channel_extractor = Some(extractor);
        self.channel_endpoint_repo = Some(repository);
        self
    }

    /// Attach cross-file channel resolution (runs after extraction).
    pub fn with_channel_resolution(mut self, resolver: Arc<dyn ChannelResolver>) -> Self {
        self.channel_resolver = Some(resolver);
        self
    }

    /// Attach the analysis store so stored analyses (clusters, communities,
    /// features) are invalidated when indexing changes the call graph.
    pub fn with_analysis_repo(mut self, analysis_repo: Arc<dyn AnalysisRepository>) -> Self {
        self.analysis_repo = Some(analysis_repo);
        self
    }

    /// Drop every stored analysis for `repository_id`.  Called after indexing
    /// changes the call graph, since stored analyses derive entirely from it.
    ///
    /// Best-effort: stored analyses are a derived cache, so a failure to drop
    /// them is logged and swallowed rather than aborting the primary indexing
    /// operation (which has, by this point, already rewritten the vector,
    /// file-hash, and call-graph data). A stale cache is corrected on the next
    /// run; a hard error here would leave the repository record dangling.
    async fn invalidate_analyses(&self, repository_id: &str, namespace: Option<&str>) {
        if let Some(analysis_repo) = &self.analysis_repo {
            if let Err(e) = analysis_repo.delete_by_repository(repository_id).await {
                warn!("Failed to invalidate stored analyses for {repository_id}: {e}");
            }
            // The namespace-wide analyses (both the file-cluster and the
            // symbol-community global runs) derive from every repository's call
            // graph, so changing any one of them stales them too. Both are cached
            // under the same per-namespace scope id (see `namespace_scope_id`);
            // `delete_by_repository` is kind-agnostic, so dropping that id clears
            // both in one call.
            if let Some(ns) = namespace {
                let scope = namespace_scope_id(ns);
                if let Err(e) = analysis_repo.delete_by_repository(&scope).await {
                    warn!("Failed to invalidate stored namespace-wide analyses for {ns}: {e}");
                }
            }
        }
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

    fn spawn_embed(
        &self,
        batch: Vec<ParseOnlyResult>,
        repository_id: String,
        scip_refs: &Arc<HashMap<String, Vec<SymbolReference>>>,
    ) -> JoinHandle<Result<EmbedResult, DomainError>> {
        tokio::spawn(do_embed(
            batch,
            repository_id,
            Arc::clone(scip_refs),
            Arc::clone(&self.embedding_service),
        ))
    }

    fn spawn_write(&self, embed: EmbedResult) -> JoinHandle<Result<FlushStats, DomainError>> {
        tokio::spawn(do_write(
            embed,
            Arc::clone(&self.vector_repo),
            Arc::clone(&self.file_hash_repo),
            Arc::clone(&self.call_graph_use_case),
            self.channel_endpoint_repo.clone(),
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
                if let Some(channel_repo) = &self.channel_endpoint_repo {
                    channel_repo.delete_by_repository(existing.id()).await?;
                }
                self.invalidate_analyses(existing.id(), existing.namespace())
                    .await;
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

        // Capture the git remote (if any) as a stable, clone-independent key so
        // later commands can auto-resolve this repository's namespace.
        let git_remote = detect_remote(absolute_path);
        if let Some(ref remote) = git_remote {
            debug!("Detected git remote '{}' for {}", remote, repo_name);
        }

        let repository = Repository::new_with_storage(
            repo_name.clone(),
            path_str.to_string(),
            store,
            namespace,
            git_remote,
        );
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
            self.channel_extractor.clone(),
            parse_concurrency,
        );

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;
        // Two-stage pipeline: embed task and write task run concurrently.
        // While batch N is being written to DuckDB, batch N+1 is being embedded.
        let mut embed_handle: Option<JoinHandle<Result<EmbedResult, DomainError>>> = None;
        let mut write_handle: Option<JoinHandle<Result<FlushStats, DomainError>>> = None;

        while let Some(maybe_result) = parse_rx.recv().await {
            progress_bar.inc(1);
            if let Some(result) = maybe_result {
                progress_bar.set_message(result.relative_path.clone());
                pending_chunk_count += result.chunks.len();
                pending.push(result);

                if pending_chunk_count >= CROSS_FILE_EMBED_BATCH {
                    // Wait for the previous embed to finish, then immediately
                    // spawn its write task (which overlaps with the next embed).
                    if let Some(task) = embed_handle.take() {
                        let embed = join_flatten(task, "Embed").await?;
                        // Drain the previous write before starting a new one.
                        if let Some(wt) = write_handle.take() {
                            let stats = join_flatten(wt, "Write").await?;
                            merge_stats(
                                stats,
                                &mut file_count,
                                &mut chunk_count,
                                &mut reference_count,
                                &mut language_stats,
                            );
                        }
                        write_handle = Some(self.spawn_write(embed));
                    }
                    let batch = std::mem::take(&mut pending);
                    pending_chunk_count = 0;
                    embed_handle =
                        Some(self.spawn_embed(batch, repository.id().to_string(), &scip_refs));
                }
            }
        }

        // Drain the pipeline tail.
        if let Some(task) = embed_handle.take() {
            let embed = join_flatten(task, "Embed").await?;
            if let Some(wt) = write_handle.take() {
                let stats = join_flatten(wt, "Write").await?;
                merge_stats(
                    stats,
                    &mut file_count,
                    &mut chunk_count,
                    &mut reference_count,
                    &mut language_stats,
                );
            }
            write_handle = Some(self.spawn_write(embed));
        }
        if let Some(wt) = write_handle.take() {
            let stats = join_flatten(wt, "Write").await?;
            merge_stats(
                stats,
                &mut file_count,
                &mut chunk_count,
                &mut reference_count,
                &mut language_stats,
            );
        }

        // Final batch: any files that didn't fill a complete batch.
        if !pending.is_empty() {
            let embed = do_embed(
                std::mem::take(&mut pending),
                repository.id().to_string(),
                Arc::clone(&scip_refs),
                Arc::clone(&self.embedding_service),
            )
            .await?;
            let stats = do_write(
                embed,
                Arc::clone(&self.vector_repo),
                Arc::clone(&self.file_hash_repo),
                Arc::clone(&self.call_graph_use_case),
                self.channel_endpoint_repo.clone(),
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

        // Cross-file channel resolution: confirm libraries via SCIP and resolve
        // config-driven channels. Runs after all endpoints are persisted.
        self.resolve_channels(repository.id(), absolute_path, &scip_refs)
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

    /// Enrich stored channel endpoints with library confirmation (SCIP) and
    /// config-value resolution (AST). A no-op when no resolver is configured.
    async fn resolve_channels(
        &self,
        repository_id: &str,
        absolute_path: &Path,
        scip_refs: &HashMap<String, Vec<SymbolReference>>,
    ) -> Result<(), DomainError> {
        let (Some(resolver), Some(repo)) = (&self.channel_resolver, &self.channel_endpoint_repo)
        else {
            return Ok(());
        };

        // Load only tree-sitter-extracted endpoints to enrich. Synthesized
        // endpoints (`source = 'config'`, originated from the call graph) are
        // derived data recomputed below, so they are dropped from storage first
        // and never reloaded — this avoids re-saving a stale channel value for a
        // call site that resolution can now do better on.
        repo.delete_synthesized_by_repository(repository_id).await?;
        let endpoints: Vec<_> = repo
            .find_by_repository(repository_id)
            .await?
            .into_iter()
            .filter(|e| e.source() == EndpointSource::TreeSitter)
            .collect();
        // Nothing to enrich *and* no call graph to originate endpoints from —
        // the resolution pass would be a no-op. When extraction found nothing
        // but SCIP has references, we still run: a wrapper/fork call site
        // (`consumer.connect({ topics })`) is originated purely from the call
        // graph by `synthesize_from_packages`.
        if endpoints.is_empty() && scip_refs.is_empty() {
            return Ok(());
        }

        // Config-candidate discovery walks and reads every JS/TS file in the
        // repo — only worth it when something needs resolving. Run it when an
        // extracted endpoint is unresolved, or when the call graph carries a
        // messaging-library reference that will originate an (unresolved)
        // endpoint needing the same config sources to resolve. Otherwise skip
        // the walk; library confirmation (which needs no candidates) still runs.
        let needs_resolution = endpoints.iter().any(|e| !e.is_resolved())
            || scip_refs
                .values()
                .flatten()
                .any(|r| r.callee_package().is_some_and(is_messaging_package));
        let (config_candidates, sources_by_file) = if needs_resolution {
            discover_config_candidates(absolute_path).await
        } else {
            (Vec::new(), HashMap::new())
        };

        let use_case = ResolveChannelsUseCase::new(resolver.clone());
        let resolved = use_case.resolve(
            repository_id,
            endpoints,
            scip_refs,
            &config_candidates,
            &sources_by_file,
        );

        // Resolution rewrites the tree-sitter set: it resolves endpoints in place
        // (same id, overwritten by the upsert) but also *replaces* some — a
        // loop-registered route (`router.get(route.path, …)`) is dropped and
        // fanned out into one new-id endpoint per route-table entry. The dropped
        // placeholder keeps its own id, so the upsert alone would leave it behind
        // as a stale `[unresolved]` row. Delete the tree-sitter set first, then
        // save the resolved result as the sole source of truth (synthesized
        // endpoints were already cleared above).
        repo.delete_tree_sitter_by_repository(repository_id).await?;
        repo.save_batch(&resolved).await?;
        debug!(
            "Resolved {} channel endpoints ({} config candidates)",
            resolved.len(),
            config_candidates.len()
        );
        Ok(())
    }

    async fn incremental_index(
        &self,
        absolute_path: &Path,
        repository: &Repository,
    ) -> Result<Repository, DomainError> {
        let start_time = Instant::now();

        // Refresh the stored git remote whenever it has changed since the last
        // index — including when it was removed (None), so a stale remote can't
        // keep auto-resolving other clones to the wrong namespace.
        let detected_remote = detect_remote(absolute_path);
        if detected_remote.as_deref() != repository.git_remote() {
            self.repository_repo
                .update_git_remote(repository.id(), detected_remote.as_deref())
                .await?;
        }

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

        // Any file change rewrites part of the call graph, so analyses derived
        // from it (clusters, communities, features) become stale. The
        // unchanged-file SCIP resync below can also rewrite edges (e.g. a
        // dependency's change altered cross-file references, or SCIP ran for the
        // first time), so this may also flip to true there.
        let mut call_graph_changed =
            !added.is_empty() || !modified.is_empty() || !deleted.is_empty();

        // Track total chunks deleted
        let mut deleted_chunk_count = 0u64;

        // Process deleted files (remove chunks, references, and endpoints)
        for path in &deleted {
            debug!("Removing deleted file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
            self.call_graph_use_case
                .delete_by_file(repository.id(), path)
                .await?;
            if let Some(channel_repo) = &self.channel_endpoint_repo {
                channel_repo
                    .delete_by_file_path(repository.id(), path)
                    .await?;
            }
        }
        if !deleted.is_empty() {
            let deleted_paths: Vec<String> = deleted.iter().map(|s| s.to_string()).collect();
            self.file_hash_repo
                .delete_by_paths(repository.id(), &deleted_paths)
                .await?;
        }

        // Process modified files (delete old chunks, references, and
        // endpoints, then re-index)
        for path in &modified {
            debug!("Re-indexing modified file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
            self.call_graph_use_case
                .delete_by_file(repository.id(), path)
                .await?;
            if let Some(channel_repo) = &self.channel_endpoint_repo {
                channel_repo
                    .delete_by_file_path(repository.id(), path)
                    .await?;
            }
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
            self.channel_extractor.clone(),
            parse_concurrency,
        );

        let mut pending: Vec<ParseOnlyResult> = Vec::new();
        let mut pending_chunk_count = 0usize;
        let mut embed_handle: Option<JoinHandle<Result<EmbedResult, DomainError>>> = None;
        let mut write_handle: Option<JoinHandle<Result<FlushStats, DomainError>>> = None;

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
                    if let Some(task) = embed_handle.take() {
                        let embed = join_flatten(task, "Embed").await?;
                        if let Some(wt) = write_handle.take() {
                            let stats = join_flatten(wt, "Write").await?;
                            merge_stats(
                                stats,
                                &mut processed_count,
                                &mut new_chunk_count,
                                &mut new_reference_count,
                                &mut language_stats,
                            );
                        }
                        write_handle = Some(self.spawn_write(embed));
                    }
                    let batch = std::mem::take(&mut pending);
                    pending_chunk_count = 0;
                    embed_handle =
                        Some(self.spawn_embed(batch, repository.id().to_string(), &scip_refs));
                }
            }
        }

        // Drain the pipeline tail.
        if let Some(task) = embed_handle.take() {
            let embed = join_flatten(task, "Embed").await?;
            if let Some(wt) = write_handle.take() {
                let stats = join_flatten(wt, "Write").await?;
                merge_stats(
                    stats,
                    &mut processed_count,
                    &mut new_chunk_count,
                    &mut new_reference_count,
                    &mut language_stats,
                );
            }
            write_handle = Some(self.spawn_write(embed));
        }
        if let Some(wt) = write_handle.take() {
            let stats = join_flatten(wt, "Write").await?;
            merge_stats(
                stats,
                &mut processed_count,
                &mut new_chunk_count,
                &mut new_reference_count,
                &mut language_stats,
            );
        }

        // Final batch.
        if !pending.is_empty() {
            let embed = do_embed(
                std::mem::take(&mut pending),
                repository.id().to_string(),
                Arc::clone(&scip_refs),
                Arc::clone(&self.embedding_service),
            )
            .await?;
            let stats = do_write(
                embed,
                Arc::clone(&self.vector_repo),
                Arc::clone(&self.file_hash_repo),
                Arc::clone(&self.call_graph_use_case),
                self.channel_endpoint_repo.clone(),
            )
            .await?;
            merge_stats(
                stats,
                &mut processed_count,
                &mut new_chunk_count,
                &mut new_reference_count,
                &mut language_stats,
            );
        }

        progress_bar.finish_and_clear();

        // SCIP references for unchanged files. Rewriting these edges also makes
        // derived analyses stale, so flag the call graph as changed whenever the
        // resync touches at least one unchanged file.
        for (relative_path, file_refs) in scip_refs.iter() {
            if new_processed_paths.contains(relative_path) {
                continue;
            }
            call_graph_changed = true;
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

        // Resolve across the full endpoint set (config resolution and library
        // confirmation can span changed and unchanged files).
        self.resolve_channels(repository.id(), absolute_path, &scip_refs)
            .await?;

        if call_graph_changed {
            self.invalidate_analyses(repository.id(), repository.namespace())
                .await;
        }

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
    /// Channel endpoints extracted from the file (empty when no channel
    /// extractor is configured or the language has no detectors).
    endpoints: Vec<ChannelEndpoint>,
}

/// Parsed batch after embedding — ready to be written to the DB.
struct EmbedResult {
    batch: Vec<ParseOnlyResult>,
    repository_id: String,
    scip_refs: Arc<HashMap<String, Vec<SymbolReference>>>,
    flat_chunks: Vec<crate::domain::CodeChunk>,
    per_file_chunk_count: Vec<usize>,
    per_file_embeddings: Vec<Option<Vec<Embedding>>>,
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

/// Await a spawned pipeline task, flattening the `JoinError` (panic) and the
/// task's own `DomainError` into a single result.
async fn join_flatten<T>(
    handle: JoinHandle<Result<T, DomainError>>,
    label: &str,
) -> Result<T, DomainError> {
    handle
        .await
        .map_err(|e| DomainError::internal(format!("{label} task panicked: {e}")))?
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
    channel_extractor: Option<Arc<dyn ChannelExtractor>>,
    concurrency: usize,
) -> mpsc::Receiver<Option<ParseOnlyResult>> {
    // Buffer enough results to absorb a full flush cycle without stalling.
    let (tx, rx) = mpsc::channel(concurrency * 8);
    tokio::spawn(async move {
        let mut stream = futures_util::stream::iter(files)
            .map(move |entry_path| {
                let parser_service = parser_service.clone();
                let channel_extractor = channel_extractor.clone();
                let abs_path = abs_path.clone();
                let repo_id = repo_id.clone();
                async move {
                    parse_only(
                        entry_path,
                        &abs_path,
                        &repo_id,
                        &*parser_service,
                        channel_extractor.as_deref(),
                    )
                    .await
                }
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

/// Phase 1 of the two-stage flush pipeline: tokenise and embed a batch.
///
/// Pure CPU work — no DB I/O.  Runs concurrently with the DB-write phase of
/// the previous batch so that embedding and persistence overlap in time.
async fn do_embed(
    batch: Vec<ParseOnlyResult>,
    repository_id: String,
    scip_refs: Arc<HashMap<String, Vec<SymbolReference>>>,
    embedding_service: Arc<dyn EmbeddingService>,
) -> Result<EmbedResult, DomainError> {
    // Flatten chunks while preserving per-file counts for later re-splitting.
    let mut flat_chunks: Vec<crate::domain::CodeChunk> = Vec::new();
    let mut per_file_chunk_count: Vec<usize> = Vec::with_capacity(batch.len());
    let batch: Vec<ParseOnlyResult> = batch
        .into_iter()
        .map(|mut r| {
            per_file_chunk_count.push(r.chunks.len());
            flat_chunks.append(&mut r.chunks);
            r
        })
        .collect();

    // Embed all chunks in one call; fall back to per-file on failure so a
    // single bad file cannot discard the whole batch.
    // When the service has embeddings disabled (--no-embeddings) every file is
    // marked successfully "embedded" with zero vectors so the write phase
    // stores its chunks without embeddings.
    let per_file_embeddings: Vec<Option<Vec<Embedding>>> =
        if !embedding_service.embeddings_enabled() || flat_chunks.is_empty() {
            per_file_chunk_count.iter().map(|_| Some(vec![])).collect()
        } else {
            match embedding_service.embed_chunks(&flat_chunks).await {
                Ok(all) => {
                    if all.len() != flat_chunks.len() {
                        return Err(DomainError::internal(format!(
                            "Embedding count mismatch: got {} embeddings for {} chunks",
                            all.len(),
                            flat_chunks.len()
                        )));
                    }
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

    Ok(EmbedResult {
        batch,
        repository_id,
        scip_refs,
        flat_chunks,
        per_file_chunk_count,
        per_file_embeddings,
    })
}

/// Phase 2 of the two-stage flush pipeline: persist an already-embedded batch.
///
/// All DB writes — delete stale, save chunks+embeddings, save call-graph refs,
/// save file hashes.  Runs concurrently with the embedding phase of the next
/// batch so that I/O and CPU work overlap.
async fn do_write(
    embed: EmbedResult,
    vector_repo: Arc<dyn VectorRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
    channel_endpoint_repo: Option<Arc<dyn ChannelEndpointRepository>>,
) -> Result<FlushStats, DomainError> {
    let EmbedResult {
        batch,
        repository_id,
        scip_refs,
        flat_chunks,
        per_file_chunk_count,
        per_file_embeddings,
    } = embed;

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
            None => continue,
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

    let mut ref_count = 0u64;
    for (i, result) in batch.iter().enumerate() {
        if per_file_embeddings[i].is_none() {
            continue;
        }
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

    // Channel endpoints: delete-then-save per file mirrors the call-graph
    // lifecycle and keeps re-indexing idempotent.
    if let Some(channel_repo) = &channel_endpoint_repo {
        for (i, result) in batch.iter().enumerate() {
            if per_file_embeddings[i].is_none() {
                continue;
            }
            channel_repo
                .delete_by_file_path(&repository_id, &result.relative_path)
                .await?;
            channel_repo.save_batch(&result.endpoints).await?;
        }
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
    channel_extractor: Option<&dyn ChannelExtractor>,
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

    // Channel extraction failures must not fail chunk indexing — log and
    // continue with no endpoints for the file.
    let endpoints = match channel_extractor {
        Some(extractor) if extractor.supports_language(language) => {
            match extractor
                .extract(&content, &relative_path, language, repo_id)
                .await
            {
                Ok(endpoints) => endpoints,
                Err(e) => {
                    warn!(
                        "Failed to extract channel endpoints from {}: {}",
                        relative_path, e
                    );
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };

    Some(ParseOnlyResult {
        relative_path,
        content_hash,
        language,
        chunks,
        endpoints,
    })
}

/// Discover the JS/TS sources the channel resolver may search: config modules
/// (for direct `this.config…` access) plus files that define or instantiate a
/// class (for the `this.<param>.<key>` constructor-parameter indirection).
///
/// Returns `(name, source)` pairs. For a config file the name is the exported
/// object (`config`); for a class file it is the class name. The resolver only
/// uses the name to look up config objects — constructor tracing scans every
/// source for `class`/`new` — so extra names are harmless. Sources are read
/// once here so the resolver never touches the filesystem.
#[allow(clippy::type_complexity)]
async fn discover_config_candidates(
    absolute_path: &Path,
) -> (Vec<(String, String)>, HashMap<String, String>) {
    let root = absolute_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut candidates = Vec::new();
        // Repo-relative path → source, so a call site can be re-read for
        // template/interface pattern inference (keys match endpoint file paths).
        let mut sources_by_file: HashMap<String, String> = HashMap::new();
        // Mirror the main indexing walker's filters so resolver discovery sees
        // exactly the files the indexer indexed — no hidden/gitignored/generated
        // files (e.g. `node_modules`, `target`) that would slow the scan and
        // match config candidates from sources the indexer never chunked.
        let walker = WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !matches!(
                Language::from_path(path),
                Language::JavaScript | Language::TypeScript
            ) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            if let Ok(relative) = path.strip_prefix(&root) {
                sources_by_file.insert(relative.to_string_lossy().to_string(), source.clone());
            }
            // Only files that carry a config object, a class definition, or a
            // `new` are useful as candidates — skip the rest to keep the
            // candidate set (and re-parsing cost) small.
            let names = candidate_names(&source);
            if names.is_empty() && !source.contains("new ") {
                continue;
            }
            if names.is_empty() {
                // Instantiation-only file: keep the source, name is unused.
                candidates.push((String::new(), source));
            } else {
                for name in names {
                    candidates.push((name, source.clone()));
                }
            }
        }
        (candidates, sources_by_file)
    })
    .await
    .unwrap_or_default()
}

/// Names a resolver candidate exposes: top-level `const <name> = {` object
/// bindings and `class <Name>` declarations. A cheap textual scan — the
/// resolver re-parses properly; here we only need the candidate names.
fn candidate_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in source.lines() {
        let line = line.trim_start().trim_start_matches("export ").trim_start();
        if let Some(rest) = line.strip_prefix("const ") {
            // `config = {` / `config: Config = {` — require an object literal.
            let name = leading_ident(rest);
            if !name.is_empty() && line.contains('{') {
                names.push(name);
            }
        } else if let Some(rest) = line.strip_prefix("class ") {
            let name = leading_ident(rest);
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    names
}

/// The leading identifier of `s` (`config: Config = {` → `config`).
fn leading_ident(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}
