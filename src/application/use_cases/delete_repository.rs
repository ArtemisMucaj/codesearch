use std::path::Path;
use std::sync::Arc;

use tracing::info;

use crate::domain::{ChunkRepository, DomainError, EmbeddingRepository, RepositoryRepository};

/// Use case for deleting an indexed repository.
pub struct DeleteRepositoryUseCase {
    repository_repo: Arc<dyn RepositoryRepository>,
    chunk_repo: Arc<dyn ChunkRepository>,
    embedding_repo: Arc<dyn EmbeddingRepository>,
}

impl DeleteRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn RepositoryRepository>,
        chunk_repo: Arc<dyn ChunkRepository>,
        embedding_repo: Arc<dyn EmbeddingRepository>,
    ) -> Self {
        Self {
            repository_repo,
            chunk_repo,
            embedding_repo,
        }
    }

    pub async fn execute(&self, id: &str) -> Result<(), DomainError> {
        let repo = self
            .repository_repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("Repository not found: {}", id)))?;

        info!("Deleting repository: {} ({})", repo.name, repo.path);

        self.embedding_repo.delete_by_repository(id).await?;
        self.chunk_repo.delete_by_repository(id).await?;
        self.repository_repo.delete(id).await?;

        info!("Repository deleted successfully");

        Ok(())
    }

    pub async fn delete_by_path(&self, path: &str) -> Result<(), DomainError> {
        // Canonicalize path to match how paths are stored during indexing
        let canonical_path = Path::new(path)
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());

        let repo = self
            .repository_repo
            .find_by_path(&canonical_path)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("Repository not found at path: {}", path)))?;

        self.execute(&repo.id).await
    }
}
