use std::path::Path;
use std::sync::Arc;

use tracing::info;

use crate::application::{RepositoryRepository, VectorRepository};
use crate::domain::DomainError;

pub struct DeleteRepositoryUseCase {
    repository_repo: Arc<dyn RepositoryRepository>,
    vector_repo: Arc<dyn VectorRepository>,
}

impl DeleteRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn RepositoryRepository>,
        vector_repo: Arc<dyn VectorRepository>,
    ) -> Self {
        Self {
            repository_repo,
            vector_repo,
        }
    }

    pub async fn execute(&self, id: &str) -> Result<(), DomainError> {
        let repo = self
            .repository_repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("Repository not found: {}", id)))?;

        info!("Deleting repository: {} ({})", repo.name(), repo.path());

        self.vector_repo.delete_by_repository(id).await?;
        self.repository_repo.delete(id).await?;

        info!("Repository deleted successfully");

        Ok(())
    }

    pub async fn delete_by_path(&self, path: &str) -> Result<(), DomainError> {
        let canonical_path = Path::new(path)
            .canonicalize()
            .map_err(|e| DomainError::InvalidInput(format!("Invalid path '{}': {}", path, e)))?
            .to_string_lossy()
            .to_string();

        let repo = self
            .repository_repo
            .find_by_path(&canonical_path)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("Repository not found at path: {}", path)))?;

        self.execute(repo.id()).await
    }
}
