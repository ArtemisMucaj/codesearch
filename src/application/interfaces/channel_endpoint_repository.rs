use async_trait::async_trait;

use crate::domain::{ChannelEndpoint, DomainError, Protocol};

/// Persistence for channel endpoints (communication call sites).
///
/// Only endpoints are stored; producer→consumer edges are always derived at
/// query time by `ChannelLinkUseCase` so re-indexing one repository can never
/// leave stale edges pointing at it.
#[async_trait]
pub trait ChannelEndpointRepository: Send + Sync {
    /// Save a batch of endpoints (upsert by id).
    async fn save_batch(&self, endpoints: &[ChannelEndpoint]) -> Result<(), DomainError>;

    /// Find all endpoints for a repository.
    async fn find_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<Vec<ChannelEndpoint>, DomainError>;

    /// Find all endpoints for a protocol across every repository.
    async fn find_by_protocol(
        &self,
        protocol: Protocol,
    ) -> Result<Vec<ChannelEndpoint>, DomainError>;

    /// Find every stored endpoint across all repositories in the namespace.
    async fn find_all(&self) -> Result<Vec<ChannelEndpoint>, DomainError>;

    /// Delete all endpoints for a specific file within a repository.
    /// Returns the number of endpoints deleted.
    async fn delete_by_file_path(
        &self,
        repository_id: &str,
        file_path: &str,
    ) -> Result<u64, DomainError>;

    /// Delete all endpoints for a repository.
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;

    /// Get statistics about the stored endpoints for a repository.
    async fn get_stats(&self, repository_id: &str) -> Result<ChannelStats, DomainError>;
}

/// Statistics about the channel endpoints stored for a repository.
#[derive(Debug, Clone, Default)]
pub struct ChannelStats {
    /// Total number of endpoints.
    pub total_endpoints: u64,
    /// Number of producer-side endpoints.
    pub producers: u64,
    /// Number of consumer-side endpoints.
    pub consumers: u64,
    /// Number of unresolved endpoints (identifier instead of literal).
    pub unresolved: u64,
    /// Breakdown by protocol.
    pub by_protocol: Vec<(String, u64)>,
}
