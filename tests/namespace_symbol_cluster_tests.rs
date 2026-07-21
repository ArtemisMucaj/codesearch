//! End-to-end tests for namespace-wide (cross-repository) **symbol** Leiden
//! community detection.
//!
//! These seed two repositories' call graphs through the real adapters
//! (in-memory DuckDB) with a cross-repository call edge, then run
//! `SymbolClusterDetectionUseCase::create_namespace_symbol_communities` and
//! assert the global partition spans both repositories and is cached under the
//! per-namespace sentinel. Unlike the file graph, symbol nodes are NOT
//! qualified — a fully-qualified symbol name is globally unique, so a caller in
//! one repository and the callee it references in another land on the same node
//! and the two call graphs join directly.

use std::sync::Arc;

use codesearch::{
    namespace_scope_id, AnalysisRepository, CallGraphRepository, CallGraphUseCase,
    DuckdbAnalysisRepository, DuckdbCallGraphRepository, DuckdbMetadataRepository, Language,
    MetadataRepository, ReferenceKind, Repository, SymbolClusterDetectionUseCase, SymbolReference,
    VectorStore,
};

/// Namespace both seeded repositories live in.
const TEST_NAMESPACE: &str = "sym-ns";

/// A resolved caller→callee symbol reference stored under `repo`. Symbol FQNs
/// are the graph node keys, so cross-repo edges form purely from matching FQNs.
fn call(caller: &str, callee: &str, repo: &str) -> SymbolReference {
    SymbolReference::new(
        Some(caller.to_string()),
        callee.to_string(),
        "src/f.php".to_string(),
        "src/f.php".to_string(),
        1,
        0,
        ReferenceKind::Call,
        Language::Php,
        repo.to_string(),
    )
}

struct Fixture {
    use_case: SymbolClusterDetectionUseCase,
    analysis_repo: Arc<dyn AnalysisRepository>,
    repo_a: Repository,
    repo_b: Repository,
}

fn ns_repo(name: &str, path: &str, namespace: &str) -> Repository {
    Repository::new_with_storage(
        name.to_string(),
        path.to_string(),
        VectorStore::default(),
        Some(namespace.to_string()),
        None,
    )
}

/// Two repositories, each an internal clique of symbols, joined by one
/// cross-repository call: `svc-a`'s `A\a0` calls `Shared\Bridge#run`, which
/// `lib` also calls internally — so `Shared\Bridge#run` is a node in both call
/// graphs and welds them.
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
    let analysis_repo: Arc<dyn AnalysisRepository> = Arc::new(
        DuckdbAnalysisRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create analysis repo"),
    );

    let repo_a = ns_repo("svc-a", "/work/svc-a", TEST_NAMESPACE);
    let repo_b = ns_repo("lib", "/work/lib", TEST_NAMESPACE);
    // A third repo in a *different* namespace, seeded with its own clique — it
    // must never appear in the global run (regression guard for the scope).
    let repo_other = ns_repo("other-svc", "/work/other-svc", "other-ns");
    metadata_repo.save(&repo_a).await.expect("save a");
    metadata_repo.save(&repo_b).await.expect("save b");
    metadata_repo.save(&repo_other).await.expect("save other");

    let mut refs = Vec::new();
    // svc-a internal clique: A\a0..a3 all call each other.
    for i in 0..4 {
        for j in 0..4 {
            if i != j {
                refs.push(call(&format!("A\\a{i}"), &format!("A\\a{j}"), repo_a.id()));
            }
        }
    }
    // lib internal clique: B\b0..b3, and every member calls the bridge symbol.
    for i in 0..4 {
        for j in 0..4 {
            if i != j {
                refs.push(call(&format!("B\\b{i}"), &format!("B\\b{j}"), repo_b.id()));
            }
        }
        refs.push(call(&format!("B\\b{i}"), "Shared\\Bridge#run", repo_b.id()));
    }
    // The cross-repo edge: svc-a calls the same bridge symbol lib uses.
    refs.push(call("A\\a0", "Shared\\Bridge#run", repo_a.id()));
    // other-ns clique — different namespace, must be excluded.
    for i in 0..4 {
        for j in 0..4 {
            if i != j {
                refs.push(call(
                    &format!("Z\\z{i}"),
                    &format!("Z\\z{j}"),
                    repo_other.id(),
                ));
            }
        }
    }
    call_graph.save_references(&refs).await.expect("seed refs");

    let use_case = SymbolClusterDetectionUseCase::new(call_graph)
        .with_storage(Arc::clone(&analysis_repo))
        .with_namespace_scope(
            TEST_NAMESPACE.to_string(),
            metadata_repo as Arc<dyn MetadataRepository>,
        );
    Fixture {
        use_case,
        analysis_repo,
        repo_a,
        repo_b,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_symbol_communities_span_both_repositories() {
    let fx = seeded_fixture().await;
    let scg = fx
        .use_case
        .create_namespace_symbol_communities(None)
        .await
        .expect("namespace symbol detection failed");

    // Cached under the per-namespace sentinel.
    assert_eq!(scg.repository_id, namespace_scope_id(TEST_NAMESPACE));

    // Every symbol from both repos appears, plus the shared bridge — and nothing
    // from the other namespace.
    let all: Vec<&String> = scg.communities.iter().flat_map(|c| &c.members).collect();
    assert!(
        all.iter().any(|m| m.starts_with("A\\")),
        "svc-a symbols present"
    );
    assert!(
        all.iter().any(|m| m.starts_with("B\\")),
        "lib symbols present"
    );
    assert!(
        all.iter().any(|m| m.as_str() == "Shared\\Bridge#run"),
        "the cross-repo bridge symbol is a node"
    );
    assert!(
        !all.iter().any(|m| m.starts_with("Z\\")),
        "a different namespace's symbols leaked into the global run: {all:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_symbol_graph_view_is_connected_across_repos() {
    let fx = seeded_fixture().await;
    let view = fx
        .use_case
        .namespace_graph_view(None)
        .await
        .expect("namespace symbol graph view failed");

    assert_eq!(view.repository_id, namespace_scope_id(TEST_NAMESPACE));
    // The bridge node connects svc-a's clique to lib's, so the graph is one
    // connected component: an edge exists from an A-symbol to the bridge and
    // from the bridge to a B-symbol.
    let idx = |fqn: &str| view.nodes.iter().position(|n| n.id == fqn);
    let bridge = idx("Shared\\Bridge#run").expect("bridge node present");
    let touches_bridge = |pred: &dyn Fn(&str) -> bool| {
        view.edges.iter().any(|e| {
            (e.source == bridge && pred(&view.nodes[e.target].id))
                || (e.target == bridge && pred(&view.nodes[e.source].id))
        })
    };
    assert!(
        touches_bridge(&|id: &str| id.starts_with("A\\")),
        "bridge links svc-a"
    );
    assert!(
        touches_bridge(&|id: &str| id.starts_with("B\\")),
        "bridge links lib"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_symbol_run_caches_and_invalidates() {
    let fx = seeded_fixture().await;
    let scope = namespace_scope_id(TEST_NAMESPACE);
    let first = fx
        .use_case
        .create_namespace_symbol_communities(None)
        .await
        .unwrap();

    // Persisted under the sentinel and round-trips to the same partition.
    let stored = fx
        .analysis_repo
        .load_symbol_community_graph(&scope)
        .await
        .expect("load stored")
        .expect("a namespace symbol run must be stored");
    assert_eq!(stored.repository_id, scope);
    let second = fx
        .use_case
        .create_namespace_symbol_communities(None)
        .await
        .unwrap();
    let ids = |g: &codesearch::SymbolCommunityGraph| {
        g.communities
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&second));

    // Invalidating the sentinel (what any re-index/delete does) clears it.
    fx.analysis_repo
        .delete_by_repository(&scope)
        .await
        .expect("invalidate");
    assert!(fx
        .analysis_repo
        .load_symbol_community_graph(&scope)
        .await
        .expect("load after invalidation")
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn per_repository_symbol_detection_is_unaffected() {
    let fx = seeded_fixture().await;
    // A per-repo run sees only that repo's symbols — the bridge is present
    // (svc-a calls it) but lib's B-symbols are not.
    let scg = fx
        .use_case
        .detect_communities(fx.repo_a.id())
        .await
        .unwrap();
    let all: Vec<&String> = scg.communities.iter().flat_map(|c| &c.members).collect();
    assert!(all.iter().any(|m| m.starts_with("A\\")));
    assert!(
        !all.iter().any(|m| m.starts_with("B\\")),
        "per-repo run is single-repo"
    );
    let _ = &fx.repo_b;
}
