use async_trait::async_trait;

use crate::domain::{DomainError, Repository};

/// Persistence for repository metadata.
#[async_trait]
pub trait RepositoryRepository: Send + Sync {
    async fn save(&self, repository: &Repository) -> Result<(), DomainError>;

    async fn find_by_id(&self, id: &str) -> Result<Option<Repository>, DomainError>;

    async fn find_by_path(&self, path: &str) -> Result<Option<Repository>, DomainError>;

    async fn list(&self) -> Result<Vec<Repository>, DomainError>;

    async fn delete(&self, id: &str) -> Result<(), DomainError>;

    async fn update_stats(
        &self,
        id: &str,
        chunk_count: u64,
        file_count: u64,
    ) -> Result<(), DomainError>;
}
