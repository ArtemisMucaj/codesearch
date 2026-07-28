//! End-to-end tests for namespace-wide (cross-repository) Leiden cluster
//! detection.
//!
//! These seed two repositories — call graph, chunks, and metadata — through the
//! real adapters (in-memory DuckDB + in-memory vector store), then run
//! `ClusterDetectionUseCase::create_namespace_clusters` and assert the global
//! partition spans both repositories with repository-qualified members and is
//! cached under the namespace sentinel. The pure qualification edge cases are
//! unit-tested inside `application::use_cases::cluster_detection`.

use std::sync::Arc;

use codesearch::{
    AnalysisRepository, CallGraphRepository, CallGraphUseCase, ClusterDetectionUseCase, CodeChunk,
    DuckdbAnalysisRepository, DuckdbCallGraphRepository, DuckdbMetadataRepository,
    FileRelationshipUseCase, InMemoryVectorRepository, Language, MetadataRepository, NodeType,
    ReferenceKind, Repository, SymbolReference, VectorRepository, NAMESPACE_SCOPE_ID,
};

/// A call reference whose callee definition site is already resolved
/// (SCIP-style): `reference_file_path` points at the callee's file.
fn resolved_ref(caller_file: &str, callee_file: &str, callee: &str, repo: &str) -> SymbolReference {
    SymbolReference::new(
        Some(format!("{caller_file}::caller")),
        callee.to_string(),
        caller_file.to_string(),
        callee_file.to_string(),
        1,
        0,
        ReferenceKind::Call,
        Language::Rust,
        repo.to_string(),
    )
}

/// A call reference left unresolved (`reference_file_path` mirrors the caller
/// file), so the file-graph builder must resolve the callee through the
/// chunk-derived symbol map — the path cross-repository edges take.
fn unresolved_ref(caller_file: &str, callee: &str, repo: &str) -> SymbolReference {
    SymbolReference::new(
        Some(format!("{caller_file}::caller")),
        callee.to_string(),
        caller_file.to_string(),
        caller_file.to_string(),
        1,
        0,
        ReferenceKind::Call,
        Language::Rust,
        repo.to_string(),
    )
}

fn chunk(file: &str, symbol: &str, repo: &str) -> CodeChunk {
    CodeChunk::new(
        file.to_string(),
        format!("fn {symbol}() {{}}"),
        1,
        3,
        Language::Rust,
        NodeType::Function,
        repo.to_string(),
    )
    .with_symbol_name(symbol)
}

struct Fixture {
    use_case: ClusterDetectionUseCase,
    analysis_repo: Arc<dyn AnalysisRepository>,
    repo_a: Repository,
    repo_b: Repository,
}

/// Two repositories, six files each, densely wired inside themselves and
/// joined by a single cross-repository call — enough structure for the global
/// Leiden run to find one community per repository.
async fn seeded_fixture() -> Fixture {
    let metadata_repo =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repo.shared_connection();
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn.clone())
            .await
            .expect("Failed to create call graph repo"),
    );
    let call_graph = Arc::new(CallGraphUseCase::new(call_graph_repo));
    let vector_repo = Arc::new(InMemoryVectorRepository::new());
    let analysis_repo: Arc<dyn AnalysisRepository> = Arc::new(
        DuckdbAnalysisRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create analysis repo"),
    );

    let repo_a = Repository::new("svc-a".to_string(), "/work/svc-a".to_string());
    let repo_b = Repository::new("lib".to_string(), "/work/lib".to_string());
    metadata_repo.save(&repo_a).await.expect("save repo_a");
    metadata_repo.save(&repo_b).await.expect("save repo_b");

    // Intra-repo cliques: every file of a repo calls every other file of it.
    let mut refs = Vec::new();
    for (repo, prefix) in [(repo_a.id(), "a"), (repo_b.id(), "b")] {
        for i in 0..6 {
            for j in 0..6 {
                if i == j {
                    continue;
                }
                refs.push(resolved_ref(
                    &format!("src/{prefix}{i}.rs"),
                    &format!("src/{prefix}{j}.rs"),
                    &format!("{prefix}{j}_sym"),
                    repo,
                ));
            }
        }
    }
    // One cross-repo call: svc-a's a0 uses `shared_util`, defined in lib's b0.
    // Unresolved on purpose so resolution goes through the symbol map.
    refs.push(unresolved_ref("src/a0.rs", "shared_util", repo_a.id()));
    call_graph.save_references(&refs).await.expect("seed refs");

    vector_repo
        .save_batch(&[chunk("src/b0.rs", "shared_util", repo_b.id())], &[])
        .await
        .expect("seed chunks");

    let file_graph = Arc::new(FileRelationshipUseCase::new(
        call_graph,
        vector_repo,
        metadata_repo,
    ));
    let use_case =
        ClusterDetectionUseCase::new(file_graph).with_storage(Arc::clone(&analysis_repo));
    Fixture {
        use_case,
        analysis_repo,
        repo_a,
        repo_b,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_clusters_span_both_repositories() {
    let fx = seeded_fixture().await;
    let cg = fx
        .use_case
        .create_namespace_clusters()
        .await
        .expect("namespace detection failed");

    assert_eq!(cg.repository_id, NAMESPACE_SCOPE_ID);
    // 6 + 6 qualified file nodes; the cross-repo edge adds no new node.
    assert_eq!(cg.total_files, 12);

    // Every member is repository-qualified.
    let members: Vec<&String> = cg.clusters.iter().flat_map(|c| &c.members).collect();
    assert!(members
        .iter()
        .all(|m| m.starts_with("svc-a:") || m.starts_with("lib:")));
    assert!(members.iter().any(|m| m.starts_with("svc-a:")));
    assert!(members.iter().any(|m| m.starts_with("lib:")));

    // Two dense cliques joined by one weak bridge: the partition must keep the
    // repositories in separate clusters.
    assert!(cg.clusters.len() >= 2, "expected ≥ 2 clusters: {cg:?}");
    for cluster in &cg.clusters {
        let in_a = cluster.members.iter().filter(|m| m.starts_with("svc-a:"));
        let in_b = cluster.members.iter().filter(|m| m.starts_with("lib:"));
        assert!(
            in_a.count() == 0 || in_b.count() == 0,
            "a cluster mixes both repositories: {:?}",
            cluster.members
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_clusters_are_cached_under_the_sentinel() {
    let fx = seeded_fixture().await;
    let first = fx.use_case.create_namespace_clusters().await.unwrap();

    // The run is persisted under the sentinel scope id…
    let stored = fx
        .analysis_repo
        .load_cluster_graph(NAMESPACE_SCOPE_ID)
        .await
        .expect("load stored")
        .expect("a namespace run must be stored");
    assert_eq!(stored.repository_id, NAMESPACE_SCOPE_ID);

    // …a second detection round-trips through the cache to the same partition…
    let second = fx.use_case.create_namespace_clusters().await.unwrap();
    let ids = |cg: &codesearch::ClusterGraph| {
        cg.clusters.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&second));

    // …and invalidating the sentinel (what any re-index/delete does) clears it.
    fx.analysis_repo
        .delete_by_repository(NAMESPACE_SCOPE_ID)
        .await
        .expect("invalidate");
    assert!(fx
        .analysis_repo
        .load_cluster_graph(NAMESPACE_SCOPE_ID)
        .await
        .expect("load after invalidation")
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn per_repository_clusters_stay_unqualified() {
    let fx = seeded_fixture().await;
    // The global run must not leak qualification or cache entries into the
    // per-repository path.
    fx.use_case.create_namespace_clusters().await.unwrap();

    let cg = fx.use_case.create_clusters(fx.repo_a.id()).await.unwrap();
    assert_eq!(cg.repository_id, fx.repo_a.id());
    assert!(cg
        .clusters
        .iter()
        .flat_map(|c| &c.members)
        .all(|m| m.starts_with("src/")));

    let cg_b = fx.use_case.create_clusters(fx.repo_b.id()).await.unwrap();
    assert_eq!(cg_b.repository_id, fx.repo_b.id());
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_graph_view_colours_nodes_by_global_cluster() {
    let fx = seeded_fixture().await;
    let view = fx
        .use_case
        .namespace_graph_view()
        .await
        .expect("namespace graph view failed");

    assert_eq!(view.repository_id, NAMESPACE_SCOPE_ID);
    assert_eq!(view.nodes.len(), 12);
    assert!(view.edge_count() >= 2, "clique edges plus the bridge");
    assert!(!view.communities.is_empty());
    // Node ids carry the repo qualifier; display labels stay the basename.
    assert!(view
        .nodes
        .iter()
        .any(|n| n.id.starts_with("svc-a:") && n.label == "a0.rs"));
}
