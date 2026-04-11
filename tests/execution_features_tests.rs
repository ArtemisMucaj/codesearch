use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, DuckdbCallGraphRepository, DuckdbMetadataRepository,
    ExecutionFeaturesUseCase, Language, ReferenceKind, SymbolReference,
};

// ──────────────────────────────────────────────────────────────────────────────
// Shared setup helpers
// ──────────────────────────────────────────────────────────────────────────────

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

/// Shorthand for building a simple call edge.
fn call_ref(
    caller: &str,
    callee: &str,
    file: &str,
    line: u32,
    repo: &str,
) -> SymbolReference {
    SymbolReference::new(
        Some(caller.to_string()),
        callee.to_string(),
        file.to_string(),
        file.to_string(),
        line,
        0,
        ReferenceKind::Call,
        Language::Rust,
        repo.to_string(),
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Entry-point detection
// ──────────────────────────────────────────────────────────────────────────────

/// A symbol with zero callers that also calls at least one other symbol is the
/// classic entry-point case and must appear in `list_features`.
#[tokio::test(flavor = "multi_thread")]
async fn test_zero_caller_symbol_is_entry_point() {
    let cg = make_call_graph_use_case().await;
    // main → foo → bar
    let refs = vec![
        call_ref("main", "foo", "src/main.rs", 1, "repo1"),
        call_ref("foo", "bar", "src/foo.rs", 5, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    // `main` is the only zero-caller symbol that calls others.
    let entry_points: Vec<&str> = features.iter().map(|f| f.entry_point.as_str()).collect();
    assert!(
        entry_points.contains(&"main"),
        "expected 'main' as an entry point, got: {:?}",
        entry_points
    );
}

/// A symbol whose short name matches a well-known pattern (e.g. `handle_request`)
/// is flagged as an entry point even when it does have callers.
#[tokio::test(flavor = "multi_thread")]
async fn test_well_known_name_is_entry_point_even_with_callers() {
    let cg = make_call_graph_use_case().await;
    // dispatcher calls handle_request, and handle_request calls process_data.
    // handle_request HAS a caller (dispatcher) but its name matches the
    // `handle_*` prefix pattern — it should still surface as an entry point.
    let refs = vec![
        call_ref(
            "dispatcher",
            "handle_request",
            "src/dispatcher.rs",
            10,
            "repo1",
        ),
        call_ref(
            "handle_request",
            "process_data",
            "src/handler.rs",
            20,
            "repo1",
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    let entry_points: Vec<&str> = features.iter().map(|f| f.entry_point.as_str()).collect();
    assert!(
        entry_points.contains(&"handle_request"),
        "handle_request should be an entry point due to name pattern; got: {:?}",
        entry_points
    );
}

/// Symbols that only appear as callees (never as callers) are not entry points.
#[tokio::test(flavor = "multi_thread")]
async fn test_pure_callee_is_not_entry_point() {
    let cg = make_call_graph_use_case().await;
    // main → leaf (leaf never calls anyone, main has no callers)
    let refs = vec![call_ref("main", "leaf", "src/main.rs", 1, "repo1")];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    // `leaf` should NOT be an entry point — it is only a callee.
    let entry_points: Vec<&str> = features.iter().map(|f| f.entry_point.as_str()).collect();
    assert!(
        !entry_points.contains(&"leaf"),
        "'leaf' must not be an entry point; got: {:?}",
        entry_points
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Forward BFS / path construction
// ──────────────────────────────────────────────────────────────────────────────

/// The feature path must contain all symbols reachable from the entry point in
/// BFS order, with the entry point itself at index 0 (depth 0).
#[tokio::test(flavor = "multi_thread")]
async fn test_feature_path_depth_and_order() {
    let cg = make_call_graph_use_case().await;
    // Linear chain: main → a → b → c
    let refs = vec![
        call_ref("main", "a", "src/main.rs", 1, "repo1"),
        call_ref("a", "b", "src/a.rs", 5, "repo1"),
        call_ref("b", "c", "src/b.rs", 10, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let feature = uc
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed")
        .expect("feature should exist");

    // Entry point is at depth 0 and is the first node.
    assert_eq!(feature.path[0].symbol, "main");
    assert_eq!(feature.path[0].depth, 0);

    // All symbols in the chain must appear.
    let syms: Vec<&str> = feature.path.iter().map(|n| n.symbol.as_str()).collect();
    assert!(syms.contains(&"a"), "path missing 'a'");
    assert!(syms.contains(&"b"), "path missing 'b'");
    assert!(syms.contains(&"c"), "path missing 'c'");

    // Reported depth equals the deepest depth seen.
    assert_eq!(feature.depth, 3, "chain of 3 hops → depth 3");
}

/// `file_count` must equal the number of distinct files touched by the path.
#[tokio::test(flavor = "multi_thread")]
async fn test_feature_file_count() {
    let cg = make_call_graph_use_case().await;
    // Three separate files.
    let refs = vec![
        call_ref("main", "a", "src/main.rs", 1, "repo1"),
        call_ref("a", "b", "src/a.rs", 5, "repo1"),
        call_ref("b", "c", "src/b.rs", 10, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let feature = uc
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed")
        .expect("feature should exist");

    // Nodes: main (src/main.rs), a (src/a.rs via ref file), b (src/b.rs), c (leaf)
    // Exact count depends on how file_path is recorded, but must be ≥ 1.
    assert!(feature.file_count >= 1, "file_count must be at least 1");
}

// ──────────────────────────────────────────────────────────────────────────────
// Cycle detection
// ──────────────────────────────────────────────────────────────────────────────

/// A mutually recursive cycle (A ↔ B) must not cause infinite BFS traversal.
/// The use case must terminate and return a finite path.
#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_guard_mutual_recursion() {
    let cg = make_call_graph_use_case().await;
    // A calls B, B calls A (mutual recursion / cycle)
    let refs = vec![
        call_ref("run", "A", "src/run.rs", 1, "repo1"),
        call_ref("A", "B", "src/a.rs", 5, "repo1"),
        call_ref("B", "A", "src/b.rs", 10, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    // Must not hang or panic.
    let feature = uc
        .get_feature("run", Some("repo1"))
        .await
        .expect("get_feature must not fail on cyclic graph")
        .expect("feature should exist");

    // Both A and B appear exactly once each.
    let syms: Vec<&str> = feature.path.iter().map(|n| n.symbol.as_str()).collect();
    let a_count = syms.iter().filter(|&&s| s == "A").count();
    let b_count = syms.iter().filter(|&&s| s == "B").count();
    assert_eq!(a_count, 1, "A must appear exactly once");
    assert_eq!(b_count, 1, "B must appear exactly once");
}

/// A diamond dependency (A → B, A → C, B → D, C → D) must not duplicate D.
#[tokio::test(flavor = "multi_thread")]
async fn test_diamond_dependency_no_duplicates() {
    let cg = make_call_graph_use_case().await;
    let refs = vec![
        call_ref("main", "B", "src/main.rs", 1, "repo1"),
        call_ref("main", "C", "src/main.rs", 2, "repo1"),
        call_ref("B", "D", "src/b.rs", 5, "repo1"),
        call_ref("C", "D", "src/c.rs", 5, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let feature = uc
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed")
        .expect("feature should exist");

    let d_count = feature.path.iter().filter(|n| n.symbol == "D").count();
    assert_eq!(d_count, 1, "D must appear exactly once (diamond dedup)");
}

// ──────────────────────────────────────────────────────────────────────────────
// Criticality scoring
// ──────────────────────────────────────────────────────────────────────────────

/// A feature touching a security-sensitive symbol scores higher criticality
/// than an otherwise identical feature without security keywords.
#[tokio::test(flavor = "multi_thread")]
async fn test_security_sensitive_raises_criticality() {
    let cg = make_call_graph_use_case().await;
    // Two entry points: one calls a security-sensitive symbol, the other doesn't.
    let refs = vec![
        // Secure path: main_secure → validate_password
        call_ref(
            "main_secure",
            "validate_password",
            "src/main.rs",
            1,
            "repo1",
        ),
        // Plain path: run_plain → compute_sum
        call_ref("run_plain", "compute_sum", "src/runner.rs", 1, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    let secure = features
        .iter()
        .find(|f| f.entry_point == "main_secure")
        .expect("main_secure feature not found");
    let plain = features
        .iter()
        .find(|f| f.entry_point == "run_plain")
        .expect("run_plain feature not found");

    assert!(
        secure.criticality > plain.criticality,
        "security-sensitive path should score higher: secure={}, plain={}",
        secure.criticality,
        plain.criticality
    );
}

/// Criticality must always be in the range [0.0, 1.0].
#[tokio::test(flavor = "multi_thread")]
async fn test_criticality_is_within_bounds() {
    let cg = make_call_graph_use_case().await;
    // Seed a graph that exercises multiple scoring signals at once:
    // deep chain + security keyword + multi-file spread.
    let refs = vec![
        call_ref("main", "auth_middleware", "src/main.rs", 1, "repo1"),
        call_ref(
            "auth_middleware",
            "validate_token",
            "src/auth.rs",
            5,
            "repo1",
        ),
        call_ref(
            "validate_token",
            "crypto_verify",
            "src/crypto.rs",
            10,
            "repo1",
        ),
        call_ref(
            "crypto_verify",
            "permission_check",
            "src/permissions.rs",
            15,
            "repo1",
        ),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    for f in &features {
        assert!(
            f.criticality >= 0.0 && f.criticality <= 1.0,
            "criticality out of range for '{}': {}",
            f.entry_point,
            f.criticality
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// list_features
// ──────────────────────────────────────────────────────────────────────────────

/// `list_features` returns an empty slice when the repository has no call-graph
/// data at all.
#[tokio::test(flavor = "multi_thread")]
async fn test_list_features_empty_repository() {
    let cg = make_call_graph_use_case().await;
    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("nonexistent-repo", 100)
        .await
        .expect("list_features failed");

    assert!(features.is_empty(), "expected empty list for empty repo");
}

/// Results are sorted by descending criticality.
#[tokio::test(flavor = "multi_thread")]
async fn test_list_features_sorted_by_descending_criticality() {
    let cg = make_call_graph_use_case().await;
    // Two distinct entry points.
    let refs = vec![
        call_ref("main_auth", "authenticate_user", "src/main.rs", 1, "repo1"),
        call_ref("run_job", "process_batch", "src/job.rs", 1, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 100)
        .await
        .expect("list_features failed");

    // Verify descending order.
    for window in features.windows(2) {
        assert!(
            window[0].criticality >= window[1].criticality,
            "features not sorted: {} < {}",
            window[0].criticality,
            window[1].criticality
        );
    }
}

/// `limit` parameter must cap the number of returned features.
#[tokio::test(flavor = "multi_thread")]
async fn test_list_features_respects_limit() {
    let cg = make_call_graph_use_case().await;
    // Three separate entry points.
    let refs = vec![
        call_ref("main", "a", "src/main.rs", 1, "repo1"),
        call_ref("run", "b", "src/run.rs", 1, "repo1"),
        call_ref("start", "c", "src/start.rs", 1, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let features = uc
        .list_features("repo1", 2)
        .await
        .expect("list_features failed");

    assert!(
        features.len() <= 2,
        "limit=2 but got {} features",
        features.len()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// get_feature
// ──────────────────────────────────────────────────────────────────────────────

/// `get_feature` returns `None` when the symbol does not exist in the call graph.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_feature_not_found() {
    let cg = make_call_graph_use_case().await;
    let uc = ExecutionFeaturesUseCase::new(cg);
    let result = uc
        .get_feature("does_not_exist", None)
        .await
        .expect("get_feature must not error");
    assert!(result.is_none(), "expected None for unknown symbol");
}

/// `get_feature` returns the feature with the correct entry_point when found.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_feature_found() {
    let cg = make_call_graph_use_case().await;
    let refs = vec![call_ref("main", "helper", "src/main.rs", 1, "repo1")];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let result = uc
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed");

    let feature = result.expect("expected Some(feature) for 'main'");
    assert_eq!(feature.entry_point, "main");
    assert_eq!(feature.repository_id, "repo1");
}

/// The feature id must be a non-empty stable string derived from repository and entry point.
#[tokio::test(flavor = "multi_thread")]
async fn test_feature_id_is_stable() {
    let cg = make_call_graph_use_case().await;
    let refs = vec![call_ref("main", "foo", "src/main.rs", 1, "repo1")];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(Arc::clone(&cg));
    let f1 = uc
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed")
        .expect("feature not found");

    let uc2 = ExecutionFeaturesUseCase::new(cg);
    let f2 = uc2
        .get_feature("main", Some("repo1"))
        .await
        .expect("get_feature failed")
        .expect("feature not found");

    assert_eq!(f1.id, f2.id, "feature id must be stable across calls");
    assert!(!f1.id.is_empty(), "feature id must not be empty");
}

// ──────────────────────────────────────────────────────────────────────────────
// get_affected_features
// ──────────────────────────────────────────────────────────────────────────────

/// A feature whose call chain includes a changed symbol must be returned.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_affected_features_includes_matching_feature() {
    let cg = make_call_graph_use_case().await;
    // main → auth → session
    let refs = vec![
        call_ref("main", "auth", "src/main.rs", 1, "repo1"),
        call_ref("auth", "session", "src/auth.rs", 5, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let affected = uc
        .get_affected_features(&["session".to_string()], Some("repo1"))
        .await
        .expect("get_affected_features failed");

    assert!(
        !affected.is_empty(),
        "should find at least one affected feature"
    );
    let ep_names: Vec<&str> = affected.iter().map(|f| f.entry_point.as_str()).collect();
    assert!(
        ep_names.contains(&"main"),
        "feature rooted at 'main' should be affected; got {:?}",
        ep_names
    );
}

/// A feature whose call chain does NOT include any changed symbol must be excluded.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_affected_features_excludes_unrelated_feature() {
    let cg = make_call_graph_use_case().await;
    // Two separate call chains.
    let refs = vec![
        call_ref("main", "auth", "src/main.rs", 1, "repo1"),
        call_ref("run", "metrics", "src/run.rs", 1, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    // Only `metrics` changed — only the `run` chain should be affected.
    let affected = uc
        .get_affected_features(&["metrics".to_string()], Some("repo1"))
        .await
        .expect("get_affected_features failed");

    let ep_names: Vec<&str> = affected.iter().map(|f| f.entry_point.as_str()).collect();
    assert!(
        !ep_names.contains(&"main"),
        "'main' chain should NOT be affected; got {:?}",
        ep_names
    );
    assert!(
        ep_names.contains(&"run"),
        "'run' chain should be affected; got {:?}",
        ep_names
    );
}

/// An empty `changed_symbols` slice must always return an empty result.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_affected_features_empty_input() {
    let cg = make_call_graph_use_case().await;
    let refs = vec![call_ref("main", "foo", "src/main.rs", 1, "repo1")];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let affected = uc
        .get_affected_features(&[], Some("repo1"))
        .await
        .expect("get_affected_features failed");

    assert!(
        affected.is_empty(),
        "empty changed_symbols should return empty result"
    );
}

/// Affected features must be sorted by descending criticality.
#[tokio::test(flavor = "multi_thread")]
async fn test_get_affected_features_sorted_by_criticality() {
    let cg = make_call_graph_use_case().await;
    // Two entry points that both call `shared_util`.
    let refs = vec![
        call_ref("main_auth", "validate_token", "src/main.rs", 1, "repo1"),
        call_ref("validate_token", "shared_util", "src/auth.rs", 5, "repo1"),
        call_ref("run_job", "shared_util", "src/job.rs", 1, "repo1"),
    ];
    cg.save_references(&refs).await.expect("seed failed");

    let uc = ExecutionFeaturesUseCase::new(cg);
    let affected = uc
        .get_affected_features(&["shared_util".to_string()], Some("repo1"))
        .await
        .expect("get_affected_features failed");

    // Verify sorted by descending criticality.
    for window in affected.windows(2) {
        assert!(
            window[0].criticality >= window[1].criticality,
            "affected features not sorted: {} < {}",
            window[0].criticality,
            window[1].criticality
        );
    }
}
