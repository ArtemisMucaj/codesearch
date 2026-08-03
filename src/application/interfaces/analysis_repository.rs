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

    /// Look up a cached LLM call-flow explanation for `(repository, symbol,
    /// model)`. `None` means none is stored (compute it). Like the other
    /// analyses, this cache is wiped by [`Self::delete_by_repository`] on
    /// re-index, so a cached explanation only ever describes the current index.
    /// Keyed on the model too, so switching models yields a fresh explanation
    /// rather than serving one written by a different model.
    async fn get_cached_explanation(
        &self,
        repository_id: &str,
        symbol: &str,
        model: &str,
    ) -> Result<Option<String>, DomainError>;

    /// Store an LLM call-flow explanation for `(repository, symbol, model)`,
    /// overwriting any previous one (what a Regenerate does).
    async fn save_explanation(
        &self,
        repository_id: &str,
        symbol: &str,
        model: &str,
        explanation: &str,
    ) -> Result<(), DomainError>;

    /// Look up cached LLM-generated display names for the given community ids.
    ///
    /// Names are keyed on the *stable, content-addressed* community id
    /// ([`crate::domain::stable_community_id`]), which is a pure function of the
    /// community's membership. This cache therefore deliberately outlives the
    /// per-repository analysis cache wiped by [`Self::delete_by_repository`]: a
    /// re-index that leaves a community's membership unchanged reuses its name
    /// for free, and a changed membership simply produces a new id (a cache
    /// miss) rather than a stale name. Missing ids are absent from the result.
    async fn get_community_names(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, DomainError>;

    /// Persist LLM-generated display names, keyed on the stable community id.
    /// Re-inserting an existing id overwrites its name.
    async fn save_community_names(&self, names: &[(String, String)]) -> Result<(), DomainError>;
}
