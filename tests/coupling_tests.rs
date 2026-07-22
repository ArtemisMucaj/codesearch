//! End-to-end tests for coupling-element detection.
//!
//! These seed a real call graph through the DuckDB repository (in-memory) and
//! run the full `CouplingDetectionUseCase` pipeline — baseline Leiden,
//! fragility probe, min-cut scoring, and ablation verification — over the
//! symbol level. The pure-algorithm edge cases (min cut, participation,
//! determinism) are unit-tested inside
//! `application::use_cases::coupling_detection`.

use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, CouplingDetectionUseCase, CouplingElementKind,
    DuckdbCallGraphRepository, DuckdbMetadataRepository, FileRelationshipUseCase, GraphLevel,
    InMemoryVectorRepository, Language, ReferenceKind, SymbolClusterDetectionUseCase,
    SymbolReference,
};

async fn make_use_case() -> (Arc<CallGraphUseCase>, CouplingDetectionUseCase) {
    let metadata_repository =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repository.shared_connection();
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );
    let call_graph = Arc::new(CallGraphUseCase::new(call_graph_repo));
    let file_graph = Arc::new(FileRelationshipUseCase::new(
        call_graph.clone(),
        Arc::new(InMemoryVectorRepository::new()),
        metadata_repository,
        // Symbol-level coupling never builds the file graph, so the namespace
        // here is only needed to satisfy the constructor.
        String::new(),
    ));
    let symbol_clusters = Arc::new(SymbolClusterDetectionUseCase::new(call_graph.clone()));
    (
        call_graph,
        CouplingDetectionUseCase::new(file_graph, symbol_clusters),
    )
}

fn call_ref(caller: &str, callee: &str, repo: &str) -> SymbolReference {
    SymbolReference::new(
        Some(caller.to_string()),
        callee.to_string(),
        "src/lib.rs".to_string(),
        "src/lib.rs".to_string(),
        1,
        0,
        ReferenceKind::Call,
        Language::Rust,
        repo.to_string(),
    )
}

/// Every ordered pair inside `group` calls each other once — a clique in the
/// undirected symbol graph.
fn clique_refs(group: &[String], repo: &str) -> Vec<SymbolReference> {
    let mut refs = Vec::new();
    for (i, caller) in group.iter().enumerate() {
        for callee in &group[i + 1..] {
            refs.push(call_ref(caller, callee, repo));
        }
    }
    refs
}

fn symbols(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}_{i}")).collect()
}

/// Seed a call graph whose baseline partition contains one community that is
/// really two 4-symbol blocks glued by a single hub symbol, plus two dense
/// unrelated cliques. The ballast cliques inflate the total edge weight so the
/// null-model penalty stays small enough for baseline Leiden to keep the glued
/// pair merged — the situation coupling detection exists to expose.
async fn seed_hub_graph(repo: &str) -> (Arc<CallGraphUseCase>, CouplingDetectionUseCase) {
    let (call_graph, use_case) = make_use_case().await;

    let block_a = symbols("order", 4);
    let block_b = symbols("billing", 4);
    let ballast_one = symbols("parse", 8);
    let ballast_two = symbols("render", 8);

    let mut refs = Vec::new();
    refs.extend(clique_refs(&block_a, repo));
    refs.extend(clique_refs(&block_b, repo));
    refs.extend(clique_refs(&ballast_one, repo));
    refs.extend(clique_refs(&ballast_two, repo));
    // The hub calls every symbol of both blocks — the only A↔B connection.
    for callee in block_a.iter().chain(&block_b) {
        refs.push(call_ref("glue_hub", callee, repo));
    }

    call_graph
        .save_references(&refs)
        .await
        .expect("seeding call graph failed");
    (call_graph, use_case)
}

#[tokio::test(flavor = "multi_thread")]
async fn detects_hub_symbol_as_coupler() {
    let (_cg, use_case) = seed_hub_graph("repo1").await;
    let report = use_case
        .detect("repo1", GraphLevel::Symbol)
        .await
        .expect("detect failed");

    assert_eq!(report.level, GraphLevel::Symbol);
    assert!(
        report.fragile_communities >= 1,
        "the glued community must be flagged fragile: {report:?}"
    );

    // Find the fragile community and its strongest node coupler.
    let community = report
        .communities
        .iter()
        .find(|c| {
            c.couplers
                .iter()
                .any(|k| k.kind == CouplingElementKind::Node)
        })
        .expect("a community with a verified node coupler");
    let node = community
        .couplers
        .iter()
        .find(|k| k.kind == CouplingElementKind::Node)
        .unwrap();
    assert_eq!(node.elements, vec!["glue_hub".to_string()]);
    assert!(node.coupling_strength > 0.0);
    assert!(node.split_probability >= 0.5);

    // The sub-blocks are the order/billing groups (the hub lands in one).
    let all_members: Vec<&String> = community
        .sub_block_a
        .iter()
        .chain(&community.sub_block_b)
        .collect();
    assert!(all_members.iter().any(|m| m.starts_with("order")));
    assert!(all_members.iter().any(|m| m.starts_with("billing")));
    assert!(
        all_members
            .iter()
            .all(|m| !m.starts_with("parse") && !m.starts_with("render")),
        "ballast cliques must not leak into the fragile community"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_repository_yields_empty_report() {
    let (_cg, use_case) = make_use_case().await;
    let report = use_case
        .detect("no-such-repo", GraphLevel::Symbol)
        .await
        .expect("detect on empty repo failed");
    assert_eq!(report.total_communities, 0);
    assert!(report.communities.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn report_is_deterministic_across_runs() {
    let (_cg, use_case) = seed_hub_graph("repo1").await;
    let first = use_case
        .detect("repo1", GraphLevel::Symbol)
        .await
        .expect("first detect failed");
    let second = use_case
        .detect("repo1", GraphLevel::Symbol)
        .await
        .expect("second detect failed");
    assert_eq!(first, second);
}
