use async_trait::async_trait;

use crate::domain::{ClusterGraph, DomainError, ExecutionFeature, SymbolCommunityGraph};

/// Persistence for derived call-graph analyses: file-level Leiden clusters,
/// symbol-level Leiden communities, and execution features.
///
/// These results are pure functions of the indexed call graph, so they are
/// stored as a cache: computed once after indexing, served from storage on
/// subsequent queries, and invalidated whenever the underlying call graph
/// changes (re-index) or the repository is deleted.
///
/// Every `load_*` method returns `Option`: `None` means the analysis has never
/// been stored for that repository (compute it), while `Some` with empty
/// contents is a valid cached result (e.g. a repository whose call graph is
/// empty).
#[async_trait]
pub trait AnalysisRepository: Send + Sync {
    /// Replace the stored file-level cluster graph for the graph's repository.
    async fn save_cluster_graph(&self, graph: &ClusterGraph) -> Result<(), DomainError>;

    /// Load the stored file-level cluster graph for a repository.
    async fn load_cluster_graph(
        &self,
        repository_id: &str,
    ) -> Result<Option<ClusterGraph>, DomainError>;

    /// Replace the stored symbol-community graph for the graph's repository.
    async fn save_symbol_community_graph(
        &self,
        graph: &SymbolCommunityGraph,
    ) -> Result<(), DomainError>;

    /// Load the stored symbol-community graph for a repository.
    async fn load_symbol_community_graph(
        &self,
        repository_id: &str,
    ) -> Result<Option<SymbolCommunityGraph>, DomainError>;

    /// Replace the stored execution features for a repository. `features` must
    /// be the complete set for the repository (not a truncated page), since
    /// loads treat the stored set as exhaustive.
    async fn save_execution_features(
        &self,
        repository_id: &str,
        features: &[ExecutionFeature],
    ) -> Result<(), DomainError>;

    /// Load all stored execution features for a repository, sorted by
    /// descending criticality.
    async fn load_execution_features(
        &self,
        repository_id: &str,
    ) -> Result<Option<Vec<ExecutionFeature>>, DomainError>;

    /// Delete every stored analysis for a repository. Called when the
    /// repository is deleted or its call graph is re-indexed.
    async fn delete_by_repository(&self, repository_id: &str) -> Result<(), DomainError>;
}
