use std::sync::Arc;

use codesearch::{
    AnalysisRepository, CallGraphQuery, CallGraphRepository, CallGraphUseCase, Cluster,
    ClusterGraph, DuckdbAnalysisRepository, DuckdbCallGraphRepository, ExecutionFeature,
    FeatureNode, Language, ReferenceKind, SymbolClusterDetectionUseCase, SymbolCommunity,
    SymbolCommunityGraph, SymbolReference,
};
use duckdb::Connection;
use tokio::sync::Mutex;

async fn create_repo() -> DuckdbAnalysisRepository {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    DuckdbAnalysisRepository::with_connection(conn)
        .await
        .unwrap()
}

fn sample_cluster_graph(repository_id: &str) -> ClusterGraph {
    // Cluster ids are UUIDs in production; scope them to the repository here so
    // fixtures for different repositories don't collide on the primary key.
    ClusterGraph {
        clusters: vec![
            Cluster {
                id: format!("{repository_id}-c1"),
                name: "auth".to_string(),
                repository_id: repository_id.to_string(),
                dominant_language: "rust".to_string(),
                size: 2,
                cohesion: 0.75,
                members: vec![
                    "src/auth/login.rs".to_string(),
                    "src/auth/mod.rs".to_string(),
                ],
            },
            Cluster {
                id: format!("{repository_id}-c2"),
                name: "search".to_string(),
                repository_id: repository_id.to_string(),
                dominant_language: "rust".to_string(),
                size: 1,
                cohesion: 0.5,
                members: vec!["src/search.rs".to_string()],
            },
        ],
        repository_id: repository_id.to_string(),
        total_files: 3,
        total_edges: 4,
    }
}

fn sample_symbol_community_graph(repository_id: &str) -> SymbolCommunityGraph {
    SymbolCommunityGraph {
        communities: vec![SymbolCommunity {
            id: "s1".to_string(),
            name: "payment".to_string(),
            repository_id: repository_id.to_string(),
            dominant_language: "php".to_string(),
            size: 2,
            cohesion: 1.0,
            members: vec![
                "svc/PaymentGateway#refund().".to_string(),
                "svc/PaymentService#charge().".to_string(),
            ],
        }],
        repository_id: repository_id.to_string(),
        total_symbols: 2,
        total_edges: 1,
    }
}

fn sample_features(repository_id: &str) -> Vec<ExecutionFeature> {
    vec![
        ExecutionFeature {
            id: format!("{repository_id}:main"),
            name: "main".to_string(),
            entry_point: "main".to_string(),
            repository_id: repository_id.to_string(),
            path: vec![
                FeatureNode {
                    symbol: "main".to_string(),
                    file_path: "src/main.rs".to_string(),
                    line: 0,
                    depth: 0,
                    repository_id: repository_id.to_string(),
                },
                FeatureNode {
                    symbol: "run".to_string(),
                    file_path: "src/lib.rs".to_string(),
                    line: 12,
                    depth: 1,
                    repository_id: repository_id.to_string(),
                },
            ],
            depth: 1,
            file_count: 2,
            reach: 2,
            criticality: 0.8,
        },
        ExecutionFeature {
            id: format!("{repository_id}:helper"),
            name: "helper".to_string(),
            entry_point: "helper".to_string(),
            repository_id: repository_id.to_string(),
            path: vec![FeatureNode {
                symbol: "helper".to_string(),
                file_path: "src/util.rs".to_string(),
                line: 3,
                depth: 0,
                repository_id: repository_id.to_string(),
            }],
            depth: 0,
            file_count: 1,
            reach: 1,
            criticality: 0.4,
        },
    ]
}

#[tokio::test]
async fn cluster_graph_roundtrip() {
    let repo = create_repo().await;

    assert!(repo.load_cluster_graph("repo-1").await.unwrap().is_none());

    let graph = sample_cluster_graph("repo-1");
    repo.save_cluster_graph(&graph).await.unwrap();

    let loaded = repo
        .load_cluster_graph("repo-1")
        .await
        .unwrap()
        .expect("stored graph");

    assert_eq!(loaded.repository_id, "repo-1");
    assert_eq!(loaded.total_files, 3);
    assert_eq!(loaded.total_edges, 4);
    assert_eq!(loaded.clusters.len(), 2);
    // Largest cluster first, members alphabetical.
    assert_eq!(loaded.clusters[0].id, "repo-1-c1");
    assert_eq!(loaded.clusters[0].name, "auth");
    assert_eq!(loaded.clusters[0].size, 2);
    assert!((loaded.clusters[0].cohesion - 0.75).abs() < 1e-6);
    assert_eq!(
        loaded.clusters[0].members,
        vec!["src/auth/login.rs", "src/auth/mod.rs"]
    );
    assert_eq!(loaded.clusters[1].id, "repo-1-c2");

    // Other repositories stay unaffected.
    assert!(repo.load_cluster_graph("repo-2").await.unwrap().is_none());
}

#[tokio::test]
async fn save_replaces_previous_cluster_graph() {
    let repo = create_repo().await;

    repo.save_cluster_graph(&sample_cluster_graph("repo-1"))
        .await
        .unwrap();

    let mut updated = sample_cluster_graph("repo-1");
    updated.clusters.truncate(1);
    updated.clusters[0].id = "c9".to_string();
    updated.total_files = 2;
    repo.save_cluster_graph(&updated).await.unwrap();

    let loaded = repo
        .load_cluster_graph("repo-1")
        .await
        .unwrap()
        .expect("stored graph");
    assert_eq!(loaded.clusters.len(), 1);
    assert_eq!(loaded.clusters[0].id, "c9");
    assert_eq!(loaded.total_files, 2);
}

#[tokio::test]
async fn empty_cluster_graph_is_distinguishable_from_missing() {
    let repo = create_repo().await;

    let empty = ClusterGraph {
        clusters: Vec::new(),
        repository_id: "repo-1".to_string(),
        total_files: 0,
        total_edges: 0,
    };
    repo.save_cluster_graph(&empty).await.unwrap();

    let loaded = repo
        .load_cluster_graph("repo-1")
        .await
        .unwrap()
        .expect("empty result is still a stored result");
    assert!(loaded.clusters.is_empty());
}

#[tokio::test]
async fn symbol_community_graph_roundtrip() {
    let repo = create_repo().await;

    assert!(repo
        .load_symbol_community_graph("repo-1")
        .await
        .unwrap()
        .is_none());

    repo.save_symbol_community_graph(&sample_symbol_community_graph("repo-1"))
        .await
        .unwrap();

    let loaded = repo
        .load_symbol_community_graph("repo-1")
        .await
        .unwrap()
        .expect("stored graph");
    assert_eq!(loaded.total_symbols, 2);
    assert_eq!(loaded.total_edges, 1);
    assert_eq!(loaded.communities.len(), 1);
    assert_eq!(loaded.communities[0].name, "payment");
    assert_eq!(loaded.communities[0].dominant_language, "php");
    assert_eq!(
        loaded.communities[0].members,
        vec![
            "svc/PaymentGateway#refund().",
            "svc/PaymentService#charge()."
        ]
    );

    // File clusters and symbol communities are stored independently.
    assert!(repo.load_cluster_graph("repo-1").await.unwrap().is_none());
}

#[tokio::test]
async fn execution_features_roundtrip() {
    let repo = create_repo().await;

    assert!(repo
        .load_execution_features("repo-1")
        .await
        .unwrap()
        .is_none());

    let features = sample_features("repo-1");
    repo.save_execution_features("repo-1", &features)
        .await
        .unwrap();

    let loaded = repo
        .load_execution_features("repo-1")
        .await
        .unwrap()
        .expect("stored features");
    assert_eq!(loaded.len(), 2);
    // Sorted by descending criticality.
    assert_eq!(loaded[0].entry_point, "main");
    assert_eq!(loaded[1].entry_point, "helper");
    // The BFS path round-trips in order.
    assert_eq!(loaded[0].path.len(), 2);
    assert_eq!(loaded[0].path[0].symbol, "main");
    assert_eq!(loaded[0].path[1].symbol, "run");
    assert_eq!(loaded[0].path[1].line, 12);
    assert_eq!(loaded[0].path[1].depth, 1);
    assert_eq!(loaded[0].depth, 1);
    assert_eq!(loaded[0].file_count, 2);
    assert!((loaded[0].criticality - 0.8).abs() < 1e-6);

    // An empty set is a stored result, not a miss.
    repo.save_execution_features("repo-1", &[]).await.unwrap();
    let empty = repo
        .load_execution_features("repo-1")
        .await
        .unwrap()
        .expect("empty stored set");
    assert!(empty.is_empty());
}

#[tokio::test]
async fn delete_by_repository_removes_all_analyses() {
    let repo = create_repo().await;

    repo.save_cluster_graph(&sample_cluster_graph("repo-1"))
        .await
        .unwrap();
    repo.save_symbol_community_graph(&sample_symbol_community_graph("repo-1"))
        .await
        .unwrap();
    repo.save_execution_features("repo-1", &sample_features("repo-1"))
        .await
        .unwrap();
    repo.save_cluster_graph(&sample_cluster_graph("repo-2"))
        .await
        .unwrap();

    repo.delete_by_repository("repo-1").await.unwrap();

    assert!(repo.load_cluster_graph("repo-1").await.unwrap().is_none());
    assert!(repo
        .load_symbol_community_graph("repo-1")
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .load_execution_features("repo-1")
        .await
        .unwrap()
        .is_none());
    // Other repositories keep their analyses.
    assert!(repo.load_cluster_graph("repo-2").await.unwrap().is_some());
}

/// Detection with storage attached must serve the stored result instead of
/// recomputing: after the call graph rows are deleted, a second detection
/// still returns the communities computed from the original graph.
#[tokio::test]
async fn symbol_cluster_detection_serves_stored_result() {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    let call_graph_repo = Arc::new(
        DuckdbCallGraphRepository::with_connection(Arc::clone(&conn))
            .await
            .unwrap(),
    );
    let analysis_repo: Arc<dyn AnalysisRepository> = Arc::new(
        DuckdbAnalysisRepository::with_connection(Arc::clone(&conn))
            .await
            .unwrap(),
    );
    let call_graph_use_case = Arc::new(CallGraphUseCase::new(call_graph_repo.clone()));

    let references: Vec<SymbolReference> = [("a", "b"), ("b", "c"), ("a", "c")]
        .iter()
        .map(|(caller, callee)| {
            SymbolReference::new(
                Some(caller.to_string()),
                callee.to_string(),
                "src/lib.rs".to_string(),
                "src/lib.rs".to_string(),
                1,
                1,
                ReferenceKind::Call,
                Language::Rust,
                "repo-1".to_string(),
            )
        })
        .collect();
    call_graph_repo.save_batch(&references).await.unwrap();

    let use_case = SymbolClusterDetectionUseCase::new(call_graph_use_case.clone())
        .with_storage(analysis_repo.clone());

    let first = use_case.detect_communities("repo-1").await.unwrap();
    assert_eq!(first.total_symbols, 3);
    assert!(!first.communities.is_empty());

    // Wipe the call graph; a recompute would now find nothing.
    call_graph_repo
        .delete_by_repository("repo-1")
        .await
        .unwrap();
    let all = call_graph_use_case
        .find_callees("a", &CallGraphQuery::new().with_repository("repo-1"))
        .await
        .unwrap();
    assert!(all.is_empty());

    let second = use_case.detect_communities("repo-1").await.unwrap();
    assert_eq!(second.total_symbols, first.total_symbols);
    assert_eq!(second.communities.len(), first.communities.len());
    assert_eq!(second.communities[0].members, first.communities[0].members);

    // Without storage the same query recomputes from the (now empty) graph.
    let uncached = SymbolClusterDetectionUseCase::new(call_graph_use_case);
    let recomputed = uncached.detect_communities("repo-1").await.unwrap();
    assert_eq!(recomputed.total_symbols, 0);
}
