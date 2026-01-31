use async_trait::async_trait;

use crate::domain::{DomainError, FileHash};

/// Persistence for file content hashes (used for incremental indexing).
#[async_trait]
pub trait FileHashRepository: Send + Sync {
    /// Save a batch of file hashes for a repository.
    async fn save_batch(&self, hashes: &[FileHash]) -> Result<(), DomainError>;

    /// Get all file hashes for a repository.
    async fn find_by_repository(&self, repository_id: &str) -> Result<Vec<FileHash>, DomainError>;

    /// Delete file hashes by their file paths within a repository.
    async fn delete_by_paths(
        &self,
        repository_id: &str,
        paths: &[String],
    ) -> Result<(), DomainError>;

    /// Delete all file hashes for a repository.
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;
}
