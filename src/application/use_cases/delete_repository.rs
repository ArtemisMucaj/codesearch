use std::path::Path;
use std::sync::Arc;

use tracing::info;

use crate::application::{
    AnalysisRepository, CallGraphUseCase, ChannelEndpointRepository, FileHashRepository,
    MetadataRepository, VectorRepository,
};
use crate::domain::{DomainError, NAMESPACE_SCOPE_ID};

pub struct DeleteRepositoryUseCase {
    repository_repo: Arc<dyn MetadataRepository>,
    vector_repo: Arc<dyn VectorRepository>,
    file_hash_repo: Arc<dyn FileHashRepository>,
    call_graph_use_case: Arc<CallGraphUseCase>,
    channel_endpoint_repo: Option<Arc<dyn ChannelEndpointRepository>>,
    /// Optional store of derived analyses (clusters, communities, features)
    /// that must be removed together with the repository.
    analysis_repo: Option<Arc<dyn AnalysisRepository>>,
}

impl DeleteRepositoryUseCase {
    pub fn new(
        repository_repo: Arc<dyn MetadataRepository>,
        vector_repo: Arc<dyn VectorRepository>,
        file_hash_repo: Arc<dyn FileHashRepository>,
        call_graph_use_case: Arc<CallGraphUseCase>,
    ) -> Self {
        Self {
            repository_repo,
            vector_repo,
            file_hash_repo,
            call_graph_use_case,
            channel_endpoint_repo: None,
            analysis_repo: None,
        }
    }

    /// Also delete stored channel endpoints when removing a repository.
    pub fn with_channel_endpoints(
        mut self,
        channel_endpoint_repo: Arc<dyn ChannelEndpointRepository>,
    ) -> Self {
        self.channel_endpoint_repo = Some(channel_endpoint_repo);
        self
    }

    /// Attach the analysis store so stored analyses are deleted with the
    /// repository.
    pub fn with_analysis_repo(mut self, analysis_repo: Arc<dyn AnalysisRepository>) -> Self {
        self.analysis_repo = Some(analysis_repo);
        self
    }

    pub async fn execute(&self, id: &str) -> Result<(), DomainError> {
        let repo = self
            .repository_repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("Repository not found: {}", id)))?;

        info!("Deleting repository: {} ({})", repo.name(), repo.path());

        self.vector_repo.delete_by_repository(id).await?;
        self.file_hash_repo.delete_by_repository(id).await?;
        self.call_graph_use_case.delete_by_repository(id).await?;
        if let Some(channel_repo) = &self.channel_endpoint_repo {
            channel_repo.delete_by_repository(id).await?;
        }
        if let Some(analysis_repo) = &self.analysis_repo {
            analysis_repo.delete_by_repository(id).await?;
            // The namespace-wide analysis spans every repository, so removing
            // one invalidates it as well.
            analysis_repo
                .delete_by_repository(NAMESPACE_SCOPE_ID)
                .await?;
        }
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
            .ok_or_else(|| {
                DomainError::not_found(format!("Repository not found at path: {}", path))
            })?;

        self.execute(repo.id()).await
    }
}
