use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, DuckdbCallGraphRepository, DuckdbMetadataRepository,
    ImpactAnalysisUseCase, Language, ReferenceKind, SymbolContextUseCase, SymbolReference,
};

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

fn call(caller: Option<&str>, callee: &str, file: &str, line: u32) -> SymbolReference {
    SymbolReference::new(
        caller.map(str::to_string),
        callee.to_string(),
        file.to_string(),
        file.to_string(),
        line,
        0,
        ReferenceKind::Call,
        Language::Rust,
        "repo1".to_string(),
    )
}

/// Seed: entry -> middle -> root_symbol
async fn seed_chain(cg: &Arc<CallGraphUseCase>) {
    let refs = vec![
        call(Some("entry"), "middle", "src/entry.rs", 1),
        call(Some("middle"), "root_symbol", "src/middle.rs", 5),
        call(Some("root_symbol"), "child", "src/root.rs", 10),
    ];
    cg.save_references(&refs).await.expect("seed failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_impact_groups_callers_by_depth() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("root_symbol", None, false)
        .await
        .expect("analyze failed");

    assert_eq!(analysis.root_symbols, vec!["root_symbol".to_string()]);
    assert_eq!(analysis.root_symbol, "root_symbol");
    assert_eq!(analysis.total_affected, 2);
    assert_eq!(analysis.max_depth_reached, 2);
    assert_eq!(analysis.by_depth[0][0].symbol, "middle");
    assert_eq!(analysis.by_depth[1][0].symbol, "entry");
}

/// The blast radius and the caller half of the symbol context are the same
/// walk over the same edges, so they must agree symbol-for-symbol.
#[tokio::test(flavor = "multi_thread")]
async fn test_impact_matches_context_callers() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;

    let analysis = ImpactAnalysisUseCase::new(Arc::clone(&cg))
        .analyze("root_symbol", None, false)
        .await
        .expect("analyze failed");
    let ctx = SymbolContextUseCase::new(cg)
        .get_context("root_symbol", None, false)
        .await
        .expect("get_context failed");

    let impact_symbols: Vec<&str> = analysis
        .by_depth
        .iter()
        .flatten()
        .map(|n| n.symbol.as_str())
        .collect();
    let context_symbols: Vec<&str> = ctx
        .callers_by_depth
        .iter()
        .flatten()
        .map(|n| n.symbol.as_str())
        .collect();

    assert_eq!(impact_symbols, context_symbols);
    assert_eq!(analysis.total_affected, ctx.total_callers);
    assert_eq!(analysis.max_depth_reached, ctx.max_caller_depth);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_impact_leaf_path_walks_back_to_root() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("root_symbol", None, false)
        .await
        .expect("analyze failed");

    let leaves = analysis.leaf_nodes();
    assert_eq!(leaves.len(), 1, "one chain, one entry point");
    assert_eq!(leaves[0].symbol, "entry");

    // Leaf-first: the outermost caller, then the one closest to the root.
    let path: Vec<&str> = analysis
        .path_for_leaf(leaves[0])
        .iter()
        .map(|n| n.symbol.as_str())
        .collect();
    assert_eq!(path, vec!["entry", "middle"]);
}

/// A module-level call site has no enclosing symbol. It must still show up in
/// the blast radius (the user needs to see it) without being traversed.
#[tokio::test(flavor = "multi_thread")]
async fn test_impact_reports_anonymous_callers() {
    let cg = make_call_graph_use_case().await;
    cg.save_references(&[
        call(None, "root_symbol", "src/main.rs", 3),
        call(None, "root_symbol", "src/other.rs", 7),
    ])
    .await
    .expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("root_symbol", None, false)
        .await
        .expect("analyze failed");

    // Deduplicated per file, not per (placeholder) symbol name.
    assert_eq!(analysis.total_affected, 2);
    assert_eq!(analysis.max_depth_reached, 1);
    assert!(analysis.by_depth[0]
        .iter()
        .all(|n| n.symbol == "<anonymous>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_impact_cycle_guard() {
    let cg = make_call_graph_use_case().await;
    cg.save_references(&[
        call(Some("A"), "B", "src/a.rs", 1),
        call(Some("B"), "A", "src/b.rs", 2),
    ])
    .await
    .expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("A", None, false)
        .await
        .expect("analyze must not loop");

    assert_eq!(analysis.total_affected, 1);
    assert_eq!(analysis.by_depth[0][0].symbol, "B");
}

/// An unindexed symbol resolves to itself, so the report is empty but still
/// keyed by what the user asked for.
#[tokio::test(flavor = "multi_thread")]
async fn test_impact_unknown_symbol_is_empty() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("no_such_symbol", None, false)
        .await
        .expect("analyze failed");

    assert_eq!(analysis.root_symbol, "no_such_symbol");
    assert_eq!(analysis.total_affected, 0);
    assert_eq!(analysis.max_depth_reached, 0);
}

/// A substring that matches no FQN exactly falls back to a `.*sub.*` regex.
#[tokio::test(flavor = "multi_thread")]
async fn test_impact_substring_falls_back_to_fuzzy_match() {
    let cg = make_call_graph_use_case().await;
    cg.save_references(&[call(
        Some("caller"),
        "module/handle_request",
        "src/caller.rs",
        1,
    )])
    .await
    .expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("handle_request", None, false)
        .await
        .expect("analyze failed");

    assert_eq!(analysis.root_symbols, vec!["module/handle_request"]);
    assert_eq!(analysis.by_depth[0][0].symbol, "caller");
}
