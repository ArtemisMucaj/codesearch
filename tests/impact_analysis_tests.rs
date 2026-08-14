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

// ── Edge-kind filtering ───────────────────────────────────────────────────
//
// `bfs` used to walk all twelve ReferenceKinds as if they were call edges, so
// a type annotation or an import became a traversal root for the next hop.
// Structural edges are still *reported* — only traversal through them stops.

fn reference(
    caller: Option<&str>,
    callee: &str,
    file: &str,
    line: u32,
    kind: ReferenceKind,
) -> SymbolReference {
    SymbolReference::new(
        caller.map(str::to_string),
        callee.to_string(),
        file.to_string(),
        file.to_string(),
        line,
        0,
        kind,
        Language::Rust,
        "repo1".to_string(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn import_edges_are_reported_but_not_traversed() {
    let cg = make_call_graph_use_case().await;
    // target <- importer (import edge) <- deep_caller (call edge)
    // `deep_caller` is reachable only *through* the import edge.
    let refs = vec![
        reference(
            Some("importer"),
            "target",
            "src/importer.rs",
            1,
            ReferenceKind::Import,
        ),
        reference(
            Some("deep_caller"),
            "importer",
            "src/deep.rs",
            2,
            ReferenceKind::Call,
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("target", None, false)
        .await
        .expect("analyze failed");

    let reached: Vec<&str> = analysis
        .by_depth
        .iter()
        .flatten()
        .map(|n| n.symbol.as_str())
        .collect();

    assert!(
        reached.contains(&"importer"),
        "the import edge itself must still be reported: {reached:?}"
    );
    assert!(
        !reached.contains(&"deep_caller"),
        "must not walk *through* an import edge: {reached:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn call_chains_still_reach_full_depth() {
    // The complement: filtering edges must not truncate real call chains.
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        reference(Some("mid"), "target", "src/mid.rs", 1, ReferenceKind::Call),
        reference(
            Some("top"),
            "mid",
            "src/top.rs",
            2,
            ReferenceKind::MethodCall,
        ),
        reference(
            Some("apex"),
            "top",
            "src/apex.rs",
            3,
            ReferenceKind::Instantiation,
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("target", None, false)
        .await
        .expect("analyze failed");

    assert_eq!(analysis.total_affected, 3, "no depth cap, no truncation");
    assert_eq!(analysis.max_depth_reached, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn trait_implementation_edges_propagate_change() {
    // `impl` transfers no control but does propagate change, so it is an
    // impact edge even though it is not an execution edge.
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        reference(
            Some("Impl"),
            "Trait",
            "src/impl.rs",
            1,
            ReferenceKind::Implementation,
        ),
        reference(Some("user"), "Impl", "src/user.rs", 2, ReferenceKind::Call),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("Trait", None, false)
        .await
        .expect("analyze failed");

    let reached: Vec<&str> = analysis
        .by_depth
        .iter()
        .flatten()
        .map(|n| n.symbol.as_str())
        .collect();
    assert!(reached.contains(&"Impl"));
    assert!(
        reached.contains(&"user"),
        "must traverse through an impl edge: {reached:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn symbol_reached_by_import_can_still_be_expanded_via_a_call() {
    // Regression guard for the visited/enqueued split, and for deciding
    // expansion *before* the reporting guard.
    //
    //   target <- hub          (import,  depth 1 — reported, not traversable)
    //   target <- direct       (call,    depth 1)
    //   direct <- hub          (call,    depth 2 — hub already reported)
    //   hub    <- upstream     (call,    depth 3)
    //
    // `hub` is first reached at depth 1 over an import edge, so it is marked
    // reported there. The real call edge to it arrives later, at depth 2, and
    // hits the `visited` guard. If expansion were decided after that guard —
    // as it reads most naturally — the `continue` would skip the enqueue and
    // `upstream` would never be found.
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        reference(
            Some("hub"),
            "target",
            "src/hub_import.rs",
            1,
            ReferenceKind::Import,
        ),
        reference(
            Some("direct"),
            "target",
            "src/direct.rs",
            2,
            ReferenceKind::Call,
        ),
        reference(
            Some("hub"),
            "direct",
            "src/hub_call.rs",
            3,
            ReferenceKind::Call,
        ),
        reference(
            Some("upstream"),
            "hub",
            "src/upstream.rs",
            4,
            ReferenceKind::Call,
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let analysis = ImpactAnalysisUseCase::new(cg)
        .analyze("target", None, false)
        .await
        .expect("analyze failed");

    let reached: Vec<&str> = analysis
        .by_depth
        .iter()
        .flatten()
        .map(|n| n.symbol.as_str())
        .collect();
    assert!(
        reached.contains(&"upstream"),
        "a call edge to an already-reported symbol must still expand it: {reached:?}"
    );
}
