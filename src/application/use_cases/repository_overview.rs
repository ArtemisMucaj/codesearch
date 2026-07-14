//! Repository overview: one report combining every analysis the tool can run
//! against a single repository.
//!
//! The use case fans out to the existing analyses — index stats, file-level
//! Leiden clusters ([`ClusterDetectionUseCase`]), symbol communities
//! ([`SymbolClusterDetectionUseCase`]), coupling elements
//! ([`CouplingDetectionUseCase`]), execution features
//! ([`ExecutionFeaturesUseCase`]), and cross-service channels
//! ([`ChannelLinkUseCase`]) — concurrently, and assembles their results into a
//! single [`OverviewReport`]. Nothing is recomputed here: each section is the
//! same data its dedicated command reports (including served-from-cache
//! behaviour), so the overview stays consistent with the drill-down commands.
//!
//! Sections degrade independently: an analysis that fails (e.g. no SCIP call
//! graph imported yet) leaves its section `None` and records the reason in
//! [`OverviewReport::skipped`] instead of failing the whole report.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    ChannelLinkOptions, ChannelLinkReport, ChannelLinkUseCase, ClusterDetectionUseCase,
    CouplingDetectionUseCase, ExecutionFeaturesUseCase, ModuleOverview,
    SymbolClusterDetectionUseCase,
};
use crate::application::{CallGraphUseCase, MetadataRepository};
use crate::domain::{
    CouplingReport, DomainError, ExecutionFeature, GraphLevel, SymbolCommunityGraph,
};

/// Default number of rows shown per ranked section (modules, communities,
/// features).
pub const DEFAULT_OVERVIEW_TOP: usize = 10;

/// Which sections to compute and how much of each to keep.
#[derive(Debug, Clone)]
pub struct OverviewOptions {
    /// Maximum entries kept in ranked sections (execution features). Cluster
    /// and community lists are returned whole — truncation is a rendering
    /// concern — but features are capped here because their computation
    /// already supports a limit.
    pub top: usize,
    pub include_modules: bool,
    pub include_symbol_communities: bool,
    pub include_couplings: bool,
    pub include_features: bool,
    pub include_channels: bool,
    /// Repository ids whose channel endpoints are joined against the target
    /// repository's (typically the rest of the current namespace). The target
    /// repository is always included.
    pub channel_scope: Vec<String>,
}

impl Default for OverviewOptions {
    fn default() -> Self {
        Self {
            top: DEFAULT_OVERVIEW_TOP,
            include_modules: true,
            include_symbol_communities: true,
            include_couplings: true,
            include_features: true,
            include_channels: true,
            channel_scope: Vec::new(),
        }
    }
}

/// Per-language slice of the index, sorted into [`OverviewStats::languages`]
/// by descending chunk count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageShare {
    pub language: String,
    pub file_count: u64,
    pub chunk_count: u64,
}

/// Index and call-graph size of the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewStats {
    pub name: String,
    pub path: String,
    pub file_count: u64,
    pub chunk_count: u64,
    /// Unix timestamp of the last index update.
    pub updated_at: i64,
    pub languages: Vec<LanguageShare>,
    pub call_graph_references: u64,
    pub call_graph_callers: u64,
    pub call_graph_callees: u64,
}

/// Channel links scoped to the target repository, plus the id→name map needed
/// to label the far end of cross-repository edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOverview {
    pub report: ChannelLinkReport,
    pub repository_names: HashMap<String, String>,
}

/// A section that could not be computed, with the reason it was skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedSection {
    pub section: String,
    pub reason: String,
}

/// The combined overview of one repository. Every section is optional: `None`
/// means the section was disabled via [`OverviewOptions`] or failed (see
/// [`Self::skipped`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewReport {
    pub repository_id: String,
    pub stats: Option<OverviewStats>,
    /// File-level Leiden clusters + inter-cluster dependencies.
    pub modules: Option<ModuleOverview>,
    /// Symbol-level Leiden communities over the call graph.
    pub symbol_communities: Option<SymbolCommunityGraph>,
    /// Coupling elements (god nodes / glue edges) at the symbol level.
    pub couplings: Option<CouplingReport>,
    /// Entry-point execution features, sorted by descending criticality.
    pub features: Option<Vec<ExecutionFeature>>,
    /// Cross-service channel links touching this repository.
    pub channels: Option<ChannelOverview>,
    /// LLM-generated executive summary. Filled in by the presentation layer
    /// (it owns the chat client); always `None` straight out of the use case.
    pub summary: Option<String>,
    pub skipped: Vec<SkippedSection>,
}

/// Use case: assemble the combined overview report for one repository.
pub struct RepositoryOverviewUseCase {
    metadata: Arc<dyn MetadataRepository>,
    call_graph: Arc<CallGraphUseCase>,
    clusters: Arc<ClusterDetectionUseCase>,
    symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
    couplings: Arc<CouplingDetectionUseCase>,
    features: Arc<ExecutionFeaturesUseCase>,
    channels: Arc<ChannelLinkUseCase>,
}

impl RepositoryOverviewUseCase {
    pub fn new(
        metadata: Arc<dyn MetadataRepository>,
        call_graph: Arc<CallGraphUseCase>,
        clusters: Arc<ClusterDetectionUseCase>,
        symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
        couplings: Arc<CouplingDetectionUseCase>,
        features: Arc<ExecutionFeaturesUseCase>,
        channels: Arc<ChannelLinkUseCase>,
    ) -> Self {
        Self {
            metadata,
            call_graph,
            clusters,
            symbol_clusters,
            couplings,
            features,
            channels,
        }
    }

    /// Compute the overview for `repository_id`. All requested sections run
    /// concurrently; individual failures are downgraded to skip notes.
    pub async fn execute(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<OverviewReport, DomainError> {
        let (stats, modules, symbol_communities, couplings, features, channels) = tokio::join!(
            self.stats_section(repository_id),
            self.modules_section(repository_id, options),
            self.symbol_communities_section(repository_id, options),
            self.couplings_section(repository_id, options),
            self.features_section(repository_id, options),
            self.channels_section(repository_id, options),
        );

        let mut skipped = Vec::new();

        // Downgrade a failed section to a skip note instead of failing the
        // whole report.
        fn unwrap_section<T>(
            skipped: &mut Vec<SkippedSection>,
            section: &str,
            result: Result<Option<T>, DomainError>,
        ) -> Option<T> {
            match result {
                Ok(value) => value,
                Err(e) => {
                    skipped.push(SkippedSection {
                        section: section.to_string(),
                        reason: e.to_string(),
                    });
                    None
                }
            }
        }

        Ok(OverviewReport {
            repository_id: repository_id.to_string(),
            stats: unwrap_section(&mut skipped, "stats", stats),
            modules: unwrap_section(&mut skipped, "modules", modules),
            symbol_communities: unwrap_section(
                &mut skipped,
                "symbol-communities",
                symbol_communities,
            ),
            couplings: unwrap_section(&mut skipped, "couplings", couplings),
            features: unwrap_section(&mut skipped, "features", features),
            channels: unwrap_section(&mut skipped, "channels", channels),
            summary: None,
            skipped,
        })
    }

    async fn stats_section(
        &self,
        repository_id: &str,
    ) -> Result<Option<OverviewStats>, DomainError> {
        let Some(repo) = self.metadata.find_by_id(repository_id).await? else {
            return Err(DomainError::not_found(format!(
                "repository '{repository_id}' is not indexed"
            )));
        };

        // Call-graph size is best-effort: an empty or missing graph is a
        // normal state (indexed without SCIP), not a stats failure.
        let cg = self
            .call_graph
            .stats(repository_id)
            .await
            .unwrap_or_default();

        let mut languages: Vec<LanguageShare> = repo
            .languages()
            .iter()
            .map(|(language, stats)| LanguageShare {
                language: language.clone(),
                file_count: stats.file_count,
                chunk_count: stats.chunk_count,
            })
            .collect();
        languages.sort_by(|a, b| b.chunk_count.cmp(&a.chunk_count));

        Ok(Some(OverviewStats {
            name: repo.name().to_string(),
            path: repo.path().to_string(),
            file_count: repo.file_count(),
            chunk_count: repo.chunk_count(),
            updated_at: repo.updated_at(),
            languages,
            call_graph_references: cg.total_references,
            call_graph_callers: cg.unique_callers,
            call_graph_callees: cg.unique_callees,
        }))
    }

    async fn modules_section(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<Option<ModuleOverview>, DomainError> {
        if !options.include_modules {
            return Ok(None);
        }
        Ok(Some(self.clusters.module_overview(repository_id).await?))
    }

    async fn symbol_communities_section(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<Option<SymbolCommunityGraph>, DomainError> {
        if !options.include_symbol_communities {
            return Ok(None);
        }
        Ok(Some(
            self.symbol_clusters
                .detect_communities(repository_id)
                .await?,
        ))
    }

    /// Coupling detection runs at the symbol level: that is where god nodes
    /// (a shared constants class, a base exception, a util grab-bag) live.
    /// File-level couplings remain available via the `couplings` command.
    async fn couplings_section(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<Option<CouplingReport>, DomainError> {
        if !options.include_couplings {
            return Ok(None);
        }
        Ok(Some(
            self.couplings
                .detect(repository_id, GraphLevel::Symbol)
                .await?,
        ))
    }

    async fn features_section(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<Option<Vec<ExecutionFeature>>, DomainError> {
        if !options.include_features {
            return Ok(None);
        }
        Ok(Some(
            self.features
                .list_features(repository_id, options.top)
                .await?,
        ))
    }

    /// Channel links are namespace-wide by nature (an edge needs both ends),
    /// so the join runs over `options.channel_scope` + the target repository
    /// and the result is filtered down to endpoints owned by the target.
    async fn channels_section(
        &self,
        repository_id: &str,
        options: &OverviewOptions,
    ) -> Result<Option<ChannelOverview>, DomainError> {
        if !options.include_channels {
            return Ok(None);
        }

        let mut scope = options.channel_scope.clone();
        if !scope.iter().any(|id| id == repository_id) {
            scope.push(repository_id.to_string());
        }

        let mut report = self
            .channels
            .link(Some(&scope), &ChannelLinkOptions::default())
            .await?;
        report.edges.retain(|e| {
            e.producer.repository_id() == repository_id
                || e.consumer.repository_id() == repository_id
        });
        report
            .unmatched_producers
            .retain(|e| e.repository_id() == repository_id);
        report
            .unmatched_consumers
            .retain(|e| e.repository_id() == repository_id);

        let repository_names: HashMap<String, String> = self
            .metadata
            .list()
            .await?
            .into_iter()
            .map(|r| (r.id().to_string(), r.name().to_string()))
            .collect();

        Ok(Some(ChannelOverview {
            report,
            repository_names,
        }))
    }
}
