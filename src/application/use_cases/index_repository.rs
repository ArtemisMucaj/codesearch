use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, info, warn};

use crate::application::{
    EmbeddingService, FileHashRepository, MetadataRepository, ParserService, VectorRepository,
};
use crate::domain::{compute_file_hash, DomainError, FileHash, Language, Repository, VectorStore};

pub struct IndexRepositoryUseCase {
    repository_repo: Arc<dyn MetadataRepository>,
    vector_repo: Arc<dyn VectorRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    parser_service: Arc<dyn ParserService>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl IndexRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn MetadataRepository>,
        vector_repo: Arc<dyn VectorRepository>,
        file_hash_repo: Arc<dyn FileHashRepository>,
        parser_service: Arc<dyn ParserService>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            repository_repo,
            vector_repo,
            file_hash_repo,
            parser_service,
            embedding_service,
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

        let progress_bar = ProgressBar::new(total_files);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                .expect("Invalid progress bar template")
                .progress_chars("#>-"),
        );

        let mut file_count = 0u64;
        let mut chunk_count = 0u64;
        let mut file_hashes = Vec::new();

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

            if chunks.is_empty() {
                progress_bar.inc(1);
                continue;
            }

            let embeddings = match self.embedding_service.embed_chunks(&chunks).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to generate embeddings for {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            self.vector_repo.save_batch(&chunks, &embeddings).await?;

            file_count += 1;
            chunk_count += chunks.len() as u64;

            debug!("Indexed {} chunks from {}", chunks.len(), relative_path);
            progress_bar.inc(1);
        }

        progress_bar.finish_with_message("done");

        // Save all file hashes
        self.file_hash_repo.save_batch(&file_hashes).await?;

        self.repository_repo
            .update_stats(repository.id(), chunk_count, file_count)
            .await?;

        let duration = start_time.elapsed();
        info!(
            "Indexing complete: {} files, {} chunks in {:.2}s",
            file_count,
            chunk_count,
            duration.as_secs_f64()
        );

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

        // Process deleted files
        for path in &deleted {
            debug!("Removing deleted file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
        }
        if !deleted.is_empty() {
            let deleted_paths: Vec<String> = deleted.iter().map(|s| s.to_string()).collect();
            self.file_hash_repo
                .delete_by_paths(repository.id(), &deleted_paths)
                .await?;
        }

        // Process modified files (delete old chunks, then re-index)
        for path in &modified {
            debug!("Re-indexing modified file: {}", path);
            deleted_chunk_count += self
                .vector_repo
                .delete_by_file_path(repository.id(), path)
                .await?;
        }

        // Process added and modified files
        let files_to_process: Vec<&String> = added.iter().chain(modified.iter()).copied().collect();
        let total_to_process = files_to_process.len() as u64;

        let progress_bar = ProgressBar::new(total_to_process);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                .expect("Invalid progress bar template")
                .progress_chars("#>-"),
        );

        let mut new_file_hashes = Vec::new();
        let mut processed_count = 0u64;
        let mut new_chunk_count = 0u64;

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

            if chunks.is_empty() {
                progress_bar.inc(1);
                continue;
            }

            let embeddings = match self.embedding_service.embed_chunks(&chunks).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to generate embeddings for {}: {}", relative_path, e);
                    progress_bar.inc(1);
                    continue;
                }
            };

            self.vector_repo.save_batch(&chunks, &embeddings).await?;

            // Only add file hash after successful indexing
            new_file_hashes.push(FileHash::new(
                relative_path.clone(),
                content_hash,
                repository.id().to_string(),
            ));

            processed_count += 1;
            new_chunk_count += chunks.len() as u64;

            debug!("Indexed {} chunks from {}", chunks.len(), relative_path);
            progress_bar.inc(1);
        }

        progress_bar.finish_with_message("done");

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

        let duration = start_time.elapsed();
        info!(
            "Incremental indexing complete: processed {} files ({} new chunks) in {:.2}s",
            processed_count,
            new_chunk_count,
            duration.as_secs_f64()
        );

        self.repository_repo
            .find_by_id(repository.id())
            .await?
            .ok_or_else(|| DomainError::internal("Repository not found after indexing"))
    }
}
