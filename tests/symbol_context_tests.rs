use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, DuckdbCallGraphRepository, DuckdbMetadataRepository,
    Language, ReferenceKind, SymbolContextUseCase, SymbolReference,
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

/// Seed: entry -> middle -> root_symbol (callers chain)
///       root_symbol -> child -> grandchild (callees chain)
async fn seed_chain(cg: &Arc<CallGraphUseCase>) {
    let refs = vec![
        // entry calls middle
        SymbolReference::new(
            Some("entry".to_string()),
            "middle".to_string(),
            "src/entry.rs".to_string(),
            "src/entry.rs".to_string(),
            1,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
        // middle calls root_symbol
        SymbolReference::new(
            Some("middle".to_string()),
            "root_symbol".to_string(),
            "src/middle.rs".to_string(),
            "src/middle.rs".to_string(),
            5,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
        // root_symbol calls child
        SymbolReference::new(
            Some("root_symbol".to_string()),
            "child".to_string(),
            "src/root.rs".to_string(),
            "src/root.rs".to_string(),
            10,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
        // child calls grandchild
        SymbolReference::new(
            Some("child".to_string()),
            "grandchild".to_string(),
            "src/child.rs".to_string(),
            "src/child.rs".to_string(),
            20,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
    ];
    cg.save_references(&refs).await.expect("Failed to seed references");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_callers_by_depth() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;
    let use_case = SymbolContextUseCase::new(cg);
    let ctx = use_case
        .get_context("root_symbol", None, false)
        .await
        .expect("get_context failed");
    assert_eq!(ctx.callers_by_depth.len(), 2, "expected 2 caller depths");
    assert_eq!(ctx.callers_by_depth[0].len(), 1);
    assert_eq!(ctx.callers_by_depth[0][0].symbol, "middle");
    assert_eq!(ctx.callers_by_depth[1].len(), 1);
    assert_eq!(ctx.callers_by_depth[1][0].symbol, "entry");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_callees_by_depth() {
    let cg = make_call_graph_use_case().await;
    seed_chain(&cg).await;
    let use_case = SymbolContextUseCase::new(cg);
    let ctx = use_case
        .get_context("root_symbol", None, false)
        .await
        .expect("get_context failed");
    assert_eq!(ctx.callees_by_depth.len(), 2, "expected 2 callee depths");
    assert_eq!(ctx.callees_by_depth[0].len(), 1);
    assert_eq!(ctx.callees_by_depth[0][0].symbol, "child");
    assert_eq!(ctx.callees_by_depth[1].len(), 1);
    assert_eq!(ctx.callees_by_depth[1][0].symbol, "grandchild");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_cycle_guard() {
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        // A calls B
        SymbolReference::new(
            Some("A".to_string()),
            "B".to_string(),
            "src/a.rs".to_string(),
            "src/a.rs".to_string(),
            1,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
        // B calls A (cycle)
        SymbolReference::new(
            Some("B".to_string()),
            "A".to_string(),
            "src/b.rs".to_string(),
            "src/b.rs".to_string(),
            2,
            0,
            ReferenceKind::Call,
            Language::Rust,
            "repo1".to_string(),
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");
    let use_case = SymbolContextUseCase::new(cg);
    let ctx = use_case
        .get_context("A", None, false)
        .await
        .expect("get_context must not loop");
    assert!(ctx.total_callees > 0);
}
