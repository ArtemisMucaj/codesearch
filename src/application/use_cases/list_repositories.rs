use std::sync::Arc;

use crate::application::MetadataRepository;
use crate::domain::{DomainError, Repository};

pub struct ListRepositoriesUseCase {
    repository_repo: Arc<dyn MetadataRepository>,
}

impl ListRepositoriesUseCase {
    pub fn new(repository_repo: Arc<dyn MetadataRepository>) -> Self {
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
