use std::sync::Arc;

use codesearch::{
    AnalysisRepository, CallGraphRepository, CallGraphUseCase, ChannelEndpointRepository,
    ChannelLinkUseCase, ClusterDetectionUseCase, CouplingDetectionUseCase,
    DuckdbAnalysisRepository, DuckdbCallGraphRepository, DuckdbChannelEndpointRepository,
    DuckdbMetadataRepository, ExecutionFeaturesUseCase, FileRelationshipUseCase,
    InMemoryVectorRepository, MetadataRepository, OverviewOptions, Repository,
    RepositoryOverviewUseCase, SymbolClusterDetectionUseCase,
};

/// Wire a [`RepositoryOverviewUseCase`] entirely from in-memory storage.
async fn setup_overview_use_case() -> (Arc<DuckdbMetadataRepository>, RepositoryOverviewUseCase) {
    let metadata =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata.shared_connection();
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create call graph repo"),
    );
    let channel_repo: Arc<dyn ChannelEndpointRepository> = Arc::new(
        DuckdbChannelEndpointRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create channel endpoint repo"),
    );
    let analysis_repo: Arc<dyn AnalysisRepository> = Arc::new(
        DuckdbAnalysisRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create analysis repo"),
    );
    let vector_repo = Arc::new(InMemoryVectorRepository::new());

    let call_graph = Arc::new(CallGraphUseCase::new(call_graph_repo));
    let file_graph = Arc::new(FileRelationshipUseCase::new(
        Arc::clone(&call_graph),
        vector_repo,
        metadata.clone() as Arc<dyn MetadataRepository>,
    ));
    let clusters = Arc::new(
        ClusterDetectionUseCase::new(Arc::clone(&file_graph))
            .with_storage(Arc::clone(&analysis_repo)),
    );
    let symbol_clusters = Arc::new(
        SymbolClusterDetectionUseCase::new(Arc::clone(&call_graph))
            .with_storage(Arc::clone(&analysis_repo)),
    );
    let couplings = Arc::new(CouplingDetectionUseCase::new(
        file_graph,
        Arc::clone(&symbol_clusters),
    ));
    let features = Arc::new(
        ExecutionFeaturesUseCase::new(Arc::clone(&call_graph)).with_storage(analysis_repo),
    );
    let channels = Arc::new(ChannelLinkUseCase::new(channel_repo));

    let use_case = RepositoryOverviewUseCase::new(
        metadata.clone() as Arc<dyn MetadataRepository>,
        call_graph,
        clusters,
        symbol_clusters,
        couplings,
        features,
        channels,
    );
    (metadata, use_case)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_overview_for_indexed_repository_populates_all_sections() {
    let (metadata, use_case) = setup_overview_use_case().await;

    let repo = Repository::new("test-repo".to_string(), "/tmp/test-repo".to_string());
    metadata.save(&repo).await.expect("Failed to save repo");

    let report = use_case
        .execute(repo.id(), &OverviewOptions::default())
        .await
        .expect("Overview should succeed for an indexed repository");

    // Nothing failed, so no section may be skipped.
    assert!(
        report.skipped.is_empty(),
        "unexpected skipped sections: {:?}",
        report.skipped
    );

    let stats = report.stats.expect("stats section should be present");
    assert_eq!(stats.name, "test-repo");
    assert_eq!(stats.path, "/tmp/test-repo");

    // With no call graph or channel endpoints the sections are present but
    // empty — graceful degradation, not failure.
    let modules = report.modules.expect("modules section should be present");
    assert!(modules.graph.clusters.is_empty());
    assert!(modules.dependencies.is_empty());
    let communities = report
        .symbol_communities
        .expect("symbol communities section should be present");
    assert!(communities.communities.is_empty());
    let features = report.features.expect("features section should be present");
    assert!(features.is_empty());
    let channels = report.channels.expect("channels section should be present");
    assert!(channels.report.edges.is_empty());
    assert!(channels
        .repository_names
        .values()
        .any(|name| name == "test-repo"));

    // The summary belongs to the presentation layer (LLM), never the use case.
    assert!(report.summary.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_overview_for_unknown_repository_skips_stats_instead_of_failing() {
    let (_metadata, use_case) = setup_overview_use_case().await;

    let report = use_case
        .execute("no-such-repo", &OverviewOptions::default())
        .await
        .expect("Overview should degrade, not fail, for an unknown repository");

    assert!(report.stats.is_none());
    assert!(
        report.skipped.iter().any(|s| s.section == "stats"),
        "stats should be reported as skipped: {:?}",
        report.skipped
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_overview_options_disable_sections() {
    let (metadata, use_case) = setup_overview_use_case().await;

    let repo = Repository::new("opts-repo".to_string(), "/tmp/opts-repo".to_string());
    metadata.save(&repo).await.expect("Failed to save repo");

    let options = OverviewOptions {
        include_modules: false,
        include_symbol_communities: false,
        include_couplings: false,
        include_features: false,
        include_channels: false,
        ..Default::default()
    };
    let report = use_case
        .execute(repo.id(), &options)
        .await
        .expect("Overview should succeed with sections disabled");

    assert!(report.stats.is_some());
    assert!(report.modules.is_none());
    assert!(report.symbol_communities.is_none());
    assert!(report.couplings.is_none());
    assert!(report.features.is_none());
    assert!(report.channels.is_none());
    // Disabled-by-option sections are not failures.
    assert!(report.skipped.is_empty());
}
