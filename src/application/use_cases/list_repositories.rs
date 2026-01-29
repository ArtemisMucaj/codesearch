use std::sync::Arc;

use crate::domain::{DomainError, Repository, RepositoryRepository};

pub struct ListRepositoriesUseCase {
    repository_repo: Arc<dyn RepositoryRepository>,
}

impl ListRepositoriesUseCase {
    pub fn new(repository_repo: Arc<dyn RepositoryRepository>) -> Self {
        Self { repository_repo }
    }

    pub async fn execute(&self) -> Result<Vec<Repository>, DomainError> {
        self.repository_repo.list().await
    }

    pub async fn get_by_id(&self, id: &str) -> Result<Option<Repository>, DomainError> {
        self.repository_repo.find_by_id(id).await
    }

    pub async fn get_by_path(&self, path: &str) -> Result<Option<Repository>, DomainError> {
        self.repository_repo.find_by_path(path).await
    }
}
