//! End-to-end tests for cross-service channel linking: index the two
//! messaging fixture repos into one store and assert the derived
//! producer→consumer edges, the unmatched report, and the incremental
//! indexing lifecycle.

use std::sync::Arc;

use codesearch::{
    CallGraphRepository, CallGraphUseCase, ChannelEndpointRepository, ChannelLinkOptions,
    ChannelLinkUseCase, ChannelRole, DuckdbCallGraphRepository, DuckdbChannelEndpointRepository,
    DuckdbFileHashRepository, DuckdbMetadataRepository, FileHashRepository,
    InMemoryVectorRepository, IndexRepositoryUseCase, MockEmbedding, Protocol,
    TreeSitterChannelExtractor, VectorStore,
};
use tempfile::tempdir;

struct TestEnv {
    index_use_case: IndexRepositoryUseCase,
    channel_repo: Arc<dyn ChannelEndpointRepository>,
}

async fn setup_test_env() -> TestEnv {
    let metadata_repository =
        Arc::new(DuckdbMetadataRepository::in_memory().expect("Failed to create DuckDB"));
    let shared_conn = metadata_repository.shared_connection();
    let file_hash_repo: Arc<dyn FileHashRepository> = Arc::new(
        DuckdbFileHashRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create file hash repo"),
    );
    let channel_repo: Arc<dyn ChannelEndpointRepository> = Arc::new(
        DuckdbChannelEndpointRepository::with_connection(Arc::clone(&shared_conn))
            .await
            .expect("Failed to create channel endpoint repo"),
    );
    let call_graph_repo: Arc<dyn CallGraphRepository> = Arc::new(
        DuckdbCallGraphRepository::with_connection(shared_conn)
            .await
            .expect("Failed to create call graph repo"),
    );

    let index_use_case = IndexRepositoryUseCase::new(
        metadata_repository,
        Arc::new(InMemoryVectorRepository::new()),
        file_hash_repo,
        Arc::new(CallGraphUseCase::new(call_graph_repo)),
        Arc::new(codesearch::TreeSitterParser::new()),
        Arc::new(MockEmbedding::new()),
    )
    .with_channel_extraction(
        Arc::new(TreeSitterChannelExtractor::new()),
        Arc::clone(&channel_repo),
    );

    TestEnv {
        index_use_case,
        channel_repo,
    }
}

async fn index(env: &TestEnv, path: &str, name: &str) -> String {
    env.index_use_case
        .execute(path, Some(name), VectorStore::InMemory, None, false)
        .await
        .unwrap_or_else(|e| panic!("Indexing {name} failed: {e}"))
        .id()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cross_repo_channel_links() {
    let env = setup_test_env().await;

    let orders_id = index(&env, "tests/fixtures/messaging/orders-service", "orders").await;
    let notifications_id = index(
        &env,
        "tests/fixtures/messaging/notification-service",
        "notifications",
    )
    .await;

    let use_case = ChannelLinkUseCase::new(Arc::clone(&env.channel_repo));
    let report = use_case
        .link(None, &ChannelLinkOptions::default())
        .await
        .expect("Channel linking failed");

    // Kafka edge: orders.created producer (Python) → consumer (kafkajs).
    let kafka_edge = report
        .edges
        .iter()
        .find(|e| e.protocol() == Protocol::Kafka)
        .expect("Expected a Kafka edge");
    assert_eq!(kafka_edge.channel(), "orders.created");
    assert_eq!(kafka_edge.producer.repository_id(), orders_id);
    assert_eq!(kafka_edge.producer.enclosing_symbol(), Some("checkout"));
    assert_eq!(kafka_edge.consumer.repository_id(), notifications_id);
    assert_eq!(kafka_edge.consumer.enclosing_symbol(), Some("start"));
    assert!(kafka_edge.is_cross_repo());
    assert!(kafka_edge.confidence > 0.0);

    // HTTP edge: axios client /api/orders/123 → Flask route /api/orders/<id>.
    let http_edge = report
        .edges
        .iter()
        .find(|e| e.protocol() == Protocol::Http)
        .expect("Expected an HTTP edge");
    assert_eq!(http_edge.channel(), "/api/orders/{}");
    assert_eq!(http_edge.producer.repository_id(), notifications_id);
    assert_eq!(http_edge.producer.host(), Some("orders-service"));
    assert_eq!(http_edge.consumer.repository_id(), orders_id);
    assert_eq!(http_edge.consumer.enclosing_symbol(), Some("get_order"));

    assert_eq!(report.edges.len(), 2, "Exactly the two fixture edges");

    // The bogus topic stays in the unmatched producer list.
    assert_eq!(report.unmatched_producers.len(), 1);
    assert_eq!(
        report.unmatched_producers[0].channel_raw(),
        "orders.audited"
    );
    assert!(report.unmatched_consumers.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repository_and_protocol_filters() {
    let env = setup_test_env().await;

    let orders_id = index(&env, "tests/fixtures/messaging/orders-service", "orders").await;
    index(
        &env,
        "tests/fixtures/messaging/notification-service",
        "notifications",
    )
    .await;

    let use_case = ChannelLinkUseCase::new(Arc::clone(&env.channel_repo));

    // Protocol filter keeps only the Kafka edge.
    let kafka_only = use_case
        .link(
            None,
            &ChannelLinkOptions {
                protocol: Some(Protocol::Kafka),
                ..Default::default()
            },
        )
        .await
        .expect("Channel linking failed");
    assert_eq!(kafka_only.edges.len(), 1);
    assert_eq!(kafka_only.edges[0].protocol(), Protocol::Kafka);

    // Restricting to a single repo leaves its endpoints dangling.
    let orders_only = use_case
        .link(Some(&[orders_id.clone()]), &ChannelLinkOptions::default())
        .await
        .expect("Channel linking failed");
    assert!(orders_only.edges.is_empty());
    assert!(orders_only
        .unmatched_producers
        .iter()
        .all(|e| e.repository_id() == orders_id));
    assert!(!orders_only.unmatched_consumers.is_empty()); // the Flask route

    // Excluding the Kafka channel by glob removes its edge.
    let excluded = use_case
        .link(
            None,
            &ChannelLinkOptions {
                exclude_channels: vec!["orders.*".to_string()],
                ..Default::default()
            },
        )
        .await
        .expect("Channel linking failed");
    assert!(excluded
        .edges
        .iter()
        .all(|e| e.protocol() != Protocol::Kafka));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_incremental_reindex_does_not_duplicate_endpoints() {
    let env = setup_test_env().await;

    // Copy the producer fixture into a temp dir so it can be modified.
    let temp_dir = tempdir().expect("Failed to create temp directory");
    let fixture =
        std::fs::read_to_string("tests/fixtures/messaging/orders-service/app.py").unwrap();
    std::fs::write(temp_dir.path().join("app.py"), &fixture).unwrap();

    let path = temp_dir.path().to_str().unwrap().to_string();
    let repo_id = index(&env, &path, "orders-tmp").await;

    let initial = env.channel_repo.find_by_repository(&repo_id).await.unwrap();
    assert!(!initial.is_empty());

    // Unchanged re-index: endpoint set must be identical.
    index(&env, &path, "orders-tmp").await;
    let unchanged = env.channel_repo.find_by_repository(&repo_id).await.unwrap();
    assert_eq!(unchanged.len(), initial.len());

    // Modified file: old endpoints replaced, no duplicates, new topic present.
    std::fs::write(
        temp_dir.path().join("app.py"),
        fixture.replace("orders.created", "orders.v2.created"),
    )
    .unwrap();
    index(&env, &path, "orders-tmp").await;

    let updated = env.channel_repo.find_by_repository(&repo_id).await.unwrap();
    assert_eq!(updated.len(), initial.len());
    let created_producers: Vec<_> = updated
        .iter()
        .filter(|e| e.role() == ChannelRole::Producer && e.channel_raw() == "orders.v2.created")
        .collect();
    assert_eq!(created_producers.len(), 1);
    assert!(!updated.iter().any(|e| e.channel_raw() == "orders.created"));

    // Deleting the file removes its endpoints on the next incremental run.
    std::fs::remove_file(temp_dir.path().join("app.py")).unwrap();
    std::fs::write(
        temp_dir.path().join("keep.py"),
        "def keep():\n    return 1\n",
    )
    .unwrap();
    index(&env, &path, "orders-tmp").await;
    let after_delete = env.channel_repo.find_by_repository(&repo_id).await.unwrap();
    assert!(after_delete.is_empty());
}
