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

    /// Find every stored endpoint. The `channel_endpoints` table is global (not
    /// namespace-scoped like `chunks`/`embeddings`), so this spans every
    /// repository in every namespace — callers that want a single namespace must
    /// scope by passing that namespace's repository ids to `find_by_repository`.
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

    /// Delete a repository's *synthesized* endpoints — those originated from the
    /// SCIP call graph rather than extracted from source (`source = 'config'`).
    ///
    /// They are pure derived data recomputed on every resolution pass, so the
    /// pass clears them first and rewrites a fresh set. This keeps the stored
    /// set free of stale rows for call sites that no longer resolve, without
    /// disturbing tree-sitter-extracted endpoints (which follow the per-file
    /// incremental lifecycle instead).
    async fn delete_synthesized_by_repository(
        &self,
        repository_id: &str,
    ) -> Result<(), DomainError>;
}
