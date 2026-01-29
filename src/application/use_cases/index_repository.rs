use std::path::Path;
use std::sync::Arc;

use ignore::WalkBuilder;
use tracing::{debug, info, warn};

use crate::domain::{
    DomainError, EmbeddingService, Language, ParserService, Repository, RepositoryRepository,
    VectorRepository,
};

pub struct IndexRepositoryUseCase {
    repository_repo: Arc<dyn RepositoryRepository>,
    vector_repo: Arc<dyn VectorRepository>,
    parser_service: Arc<dyn ParserService>,
    embedding_service: Arc<dyn EmbeddingService>,
}

impl IndexRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn RepositoryRepository>,
        vector_repo: Arc<dyn VectorRepository>,
        parser_service: Arc<dyn ParserService>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            repository_repo,
            vector_repo,
            parser_service,
            embedding_service,
        }
    }

    pub async fn execute(&self, path: &str, name: Option<&str>) -> Result<Repository, DomainError> {
        let path = Path::new(path);
        let absolute_path = path
            .canonicalize()
            .map_err(|e| DomainError::InvalidInput(format!("Invalid path: {}", e)))?;

        let path_str = absolute_path.to_string_lossy().to_string();

        if let Some(existing) = self.repository_repo.find_by_path(&path_str).await? {
            info!("Repository already indexed, re-indexing: {}", path_str);
            self.vector_repo.delete_by_repository(&existing.id).await?;
            self.repository_repo.delete(&existing.id).await?;
        }

        let repo_name = name
            .map(String::from)
            .unwrap_or_else(|| {
                absolute_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        let repository = Repository::new(repo_name.clone(), path_str.clone());
        self.repository_repo.save(&repository).await?;

        info!("Indexing repository: {} at {}", repo_name, path_str);

        let mut file_count = 0u64;
        let mut chunk_count = 0u64;

        let walker = WalkBuilder::new(&absolute_path)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker.flatten() {
            let entry_path = entry.path();

            if !entry_path.is_file() {
                continue;
            }

            let language = Language::from_path(entry_path);
            if language == Language::Unknown || !self.parser_service.supports_language(language) {
                continue;
            }

            let relative_path = entry_path
                .strip_prefix(&absolute_path)
                .unwrap_or(entry_path)
                .to_string_lossy()
                .to_string();

            debug!("Processing file: {}", relative_path);

            let content = match tokio::fs::read_to_string(entry_path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read file {}: {}", relative_path, e);
                    continue;
                }
            };

            let chunks = match self
                .parser_service
                .parse_file(&content, &relative_path, language, &repository.id)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to parse file {}: {}", relative_path, e);
                    continue;
                }
            };

            if chunks.is_empty() {
                continue;
            }

            let embeddings = match self.embedding_service.embed_chunks(&chunks).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to generate embeddings for {}: {}", relative_path, e);
                    continue;
                }
            };

            self.vector_repo.save_batch(&chunks, &embeddings).await?;

            file_count += 1;
            chunk_count += chunks.len() as u64;

            debug!("Indexed {} chunks from {}", chunks.len(), relative_path);
        }

        self.repository_repo
            .update_stats(&repository.id, chunk_count, file_count)
            .await?;

        info!("Indexing complete: {} files, {} chunks", file_count, chunk_count);

        self.repository_repo
            .find_by_id(&repository.id)
            .await?
            .ok_or_else(|| DomainError::internal("Repository not found after indexing"))
    }
}
