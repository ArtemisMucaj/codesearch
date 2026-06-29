//! End-to-end tests for the graph visualization exporters.
//!
//! These seed a real call graph, build a [`GraphView`] through the actual
//! symbol-community use case, then render every format and assert the output is
//! well-formed. The pure rendering edge cases (determinism, aggregation, palette)
//! are unit-tested inside `application::use_cases::visualize_graph`.

use std::sync::Arc;

use codesearch::{
    aggregate, render, CallGraphRepository, CallGraphUseCase, DuckdbCallGraphRepository,
    DuckdbMetadataRepository, Language, ReferenceKind, SymbolClusterDetectionUseCase,
    SymbolReference, VizFormat,
};
use serde_json::Value;

async fn make_call_graph_use_case() -> Arc<CallGraphUseCase> {
    let metadata_repository =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repository.shared_connection();
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );
    Arc::new(CallGraphUseCase::new(call_graph_repo))
}

fn call_ref(caller: &str, callee: &str, file: &str, repo: &str) -> SymbolReference {
    SymbolReference::new(
        Some(caller.to_string()),
        callee.to_string(),
        file.to_string(),
        file.to_string(),
        1,
        0,
        ReferenceKind::Call,
        Language::Rust,
        repo.to_string(),
    )
}

/// Two tightly-knit call clusters joined by a single bridge — enough structure
/// for Leiden to find more than one community.
async fn seeded_use_case() -> SymbolClusterDetectionUseCase {
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        // cluster A
        call_ref("a_main", "a_one", "src/a.rs", "repo1"),
        call_ref("a_one", "a_two", "src/a.rs", "repo1"),
        call_ref("a_two", "a_main", "src/a.rs", "repo1"),
        // cluster B
        call_ref("b_main", "b_one", "src/b.rs", "repo1"),
        call_ref("b_one", "b_two", "src/b.rs", "repo1"),
        call_ref("b_two", "b_main", "src/b.rs", "repo1"),
        // bridge
        call_ref("a_main", "b_main", "src/a.rs", "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");
    SymbolClusterDetectionUseCase::new(cg)
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_view_reflects_seeded_call_graph() {
    let uc = seeded_use_case().await;
    let view = uc.graph_view("repo1").await.expect("graph_view failed");

    assert_eq!(view.node_count(), 6, "six distinct symbols");
    assert!(view.edge_count() >= 6, "at least the intra-cluster edges");
    // Every node's degree was populated.
    assert!(view.nodes.iter().all(|n| n.degree > 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn html_export_is_self_contained_and_coloured() {
    let uc = seeded_use_case().await;
    let view = uc.graph_view("repo1").await.unwrap();
    let html = render(&view, VizFormat::Html);

    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("vis-network"));
    assert!(html.contains("RAW_NODES"));
    assert!(html.contains("#4E79A7"), "first palette colour present");
    // The embedded data assignment must be present and on its own line.
    assert!(html.contains("const RAW_EDGES = "));
}

#[tokio::test(flavor = "multi_thread")]
async fn svg_export_has_nodes_and_edges() {
    let uc = seeded_use_case().await;
    let view = uc.graph_view("repo1").await.unwrap();
    let svg = render(&view, VizFormat::Svg);

    assert!(svg.starts_with("<svg"));
    assert!(svg.trim_end().ends_with("</svg>"));
    assert!(svg.matches("<circle").count() >= view.node_count());
    assert_eq!(svg.matches("<line").count(), view.edge_count());
}

#[tokio::test(flavor = "multi_thread")]
async fn canvas_is_valid_json() {
    let uc = seeded_use_case().await;
    let view = uc.graph_view("repo1").await.unwrap();
    let canvas = render(&view, VizFormat::Canvas);

    let parsed: Value = serde_json::from_str(&canvas).expect("canvas must be valid JSON");
    let nodes = parsed["nodes"].as_array().unwrap();
    // groups (one per community) + one card per symbol.
    let groups = nodes.iter().filter(|n| n["type"] == "group").count();
    let cards = nodes.iter().filter(|n| n["type"] == "text").count();
    assert_eq!(groups, view.communities.len());
    assert_eq!(cards, view.node_count());
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregate_then_render_collapses_to_meta_graph() {
    let uc = seeded_use_case().await;
    let view = uc.graph_view("repo1").await.unwrap();
    let meta = aggregate(&view);

    assert_eq!(meta.nodes.len(), view.communities.len());
    assert!(meta.node_count() < view.node_count());
    // Still renders to valid HTML.
    let html = render(&meta, VizFormat::Html);
    assert!(html.contains("RAW_NODES"));
}
