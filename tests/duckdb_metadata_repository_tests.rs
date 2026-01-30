use std::sync::Arc;

use codesearch::{DuckdbMetadataRepository, Repository, MetadataRepository};
use tempfile::tempdir;

#[tokio::test]
async fn duckdb_repository_adapter_roundtrip_save_and_find() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");

    let repo_store = Arc::new(DuckdbMetadataRepository::new(&db_path).expect("duckdb init"));

    let repo = Repository::new("my-repo".to_string(), "/tmp/my-repo".to_string());
    repo_store.save(&repo).await.expect("save");

    let by_id = repo_store
        .find_by_id(repo.id())
        .await
        .expect("find_by_id")
        .expect("repo exists");
    assert_eq!(by_id.id(), repo.id());
    assert_eq!(by_id.name(), repo.name());
    assert_eq!(by_id.path(), repo.path());

    let by_path = repo_store
        .find_by_path(repo.path())
        .await
        .expect("find_by_path")
        .expect("repo exists");
    assert_eq!(by_path.id(), repo.id());
}

#[tokio::test]
async fn duckdb_repository_adapter_update_stats_and_list_and_delete() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("codesearch.duckdb");

    let repo_store = Arc::new(DuckdbMetadataRepository::new(&db_path).expect("duckdb init"));

    let repo1 = Repository::new("b".to_string(), "/tmp/b".to_string());
    let repo2 = Repository::new("a".to_string(), "/tmp/a".to_string());

    repo_store.save(&repo1).await.expect("save repo1");
    repo_store.save(&repo2).await.expect("save repo2");

    // list ordered by name (a then b)
    let repos = repo_store.list().await.expect("list");
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].name(), "a");
    assert_eq!(repos[1].name(), "b");

    repo_store
        .update_stats(repo2.id(), 123, 45)
        .await
        .expect("update_stats");
    let updated = repo_store
        .find_by_id(repo2.id())
        .await
        .expect("find_by_id")
        .expect("repo exists");
    assert_eq!(updated.chunk_count(), 123);
    assert_eq!(updated.file_count(), 45);

    repo_store.delete(repo1.id()).await.expect("delete");
    let remaining = repo_store.list().await.expect("list");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id(), repo2.id());
}
