//! Incremental indexing: change detection must not confuse "could not read"
//! with "deleted".
//!
//! The walk phase is the sole input to change detection, so a file skipped
//! because it failed to read was classified as deleted and purged — chunks,
//! embeddings, call-graph edges and its hash record — while it was still on
//! disk. The damage self-heals on the next run (the missing hash record makes
//! it look newly added), which is exactly why it went unnoticed: between the
//! two runs, `search`, `impact` and `context` answer from an index that is
//! silently missing real code.
//!
//! These tests use `\xff\xfe` invalid UTF-8 to make a file unreadable rather
//! than `chmod`, which behaves differently as root and on Windows.

use std::sync::Arc;

use codesearch::{
    Container, ContainerConfig, EmbeddingTarget, LlmTarget, RerankingTarget, VectorStore,
};
use tempfile::{tempdir, TempDir};

/// In-memory container: in-memory vector storage, mock embeddings, no network.
async fn test_container() -> (Arc<Container>, TempDir) {
    let dir = tempdir().expect("failed to create temp dir");
    let config = ContainerConfig {
        data_dir: dir.path().to_string_lossy().to_string(),
        mock_embeddings: true,
        namespace: "search".to_string(),
        memory_storage: true,
        no_rerank: true,
        no_embeddings: false,
        read_only: false,
        expand_query: false,
        embedding_target: EmbeddingTarget::Onnx,
        reranking_target: RerankingTarget::Onnx,
        llm_target: LlmTarget::Anthropic,
        embedding_model: None,
        embedding_dimensions: 384,
        parse_concurrency: 1,
    };
    let container = Arc::new(
        Container::new(config)
            .await
            .expect("failed to build in-memory container"),
    );
    (container, dir)
}

/// A repository with two parseable Rust files, one of which (`keep.rs`) the
/// tests then make unreadable.
fn make_repo() -> TempDir {
    let repo = tempdir().expect("failed to create repo dir");
    std::fs::write(
        repo.path().join("keep.rs"),
        "pub fn kept_helper() -> u32 { 42 }\n\
         pub fn kept_caller() -> u32 { kept_helper() }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("other.rs"),
        "pub fn other_fn() -> u32 { 7 }\n",
    )
    .unwrap();
    repo
}

/// Overwrite a file with bytes that are not valid UTF-8, so `read_to_string`
/// fails while the file remains present on disk.
fn make_unreadable(path: &std::path::Path) {
    std::fs::write(path, [0xff, 0xfe, 0x00, 0x41]).unwrap();
}

#[tokio::test]
async fn unreadable_file_is_not_treated_as_deleted() {
    let (container, _data) = test_container().await;
    let repo = make_repo();
    let repo_path = repo.path().to_string_lossy().to_string();

    let indexed = container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("initial index failed");
    let repo_id = indexed.id().to_string();

    let vector_repo = container.vector_repository();
    let before = vector_repo
        .find_chunks_by_file(&repo_id, "keep.rs")
        .await
        .unwrap();
    assert!(
        !before.is_empty(),
        "precondition: keep.rs must be indexed before we break it"
    );

    make_unreadable(&repo.path().join("keep.rs"));

    container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("incremental index failed");

    // 1. Chunks survive.
    let after = vector_repo
        .find_chunks_by_file(&repo_id, "keep.rs")
        .await
        .unwrap();
    assert!(
        !after.is_empty(),
        "chunks for keep.rs were purged though the file is still on disk"
    );

    // Call-graph edges are deleted in the same `deleted` loop as the chunks
    // above, so the chunk assertion already covers that path. Asserting on
    // them here is not possible end-to-end: call-graph extraction produces no
    // references for a fixture of this size, so the assertion would pass
    // vacuously whether or not the fix were present.

    // 3. The hash record survives. Without it the file is re-added as new on
    //    the next run, which is what made the data loss self-healing and so
    //    easy to miss.
    let hashes = container
        .file_hash_repository()
        .find_by_repository(&repo_id)
        .await
        .unwrap();
    assert!(
        hashes.iter().any(|h| h.file_path() == "keep.rs"),
        "hash record for keep.rs was deleted"
    );
}

#[tokio::test]
async fn genuinely_deleted_file_is_still_removed() {
    // The complement of the test above: retaining unreadable files must not
    // break real deletion.
    let (container, _data) = test_container().await;
    let repo = make_repo();
    let repo_path = repo.path().to_string_lossy().to_string();

    let indexed = container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("initial index failed");
    let repo_id = indexed.id().to_string();

    std::fs::remove_file(repo.path().join("keep.rs")).unwrap();

    container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("incremental index failed");

    let chunks = container
        .vector_repository()
        .find_chunks_by_file(&repo_id, "keep.rs")
        .await
        .unwrap();
    assert!(chunks.is_empty(), "deleted file's chunks must be removed");

    let hashes = container
        .file_hash_repository()
        .find_by_repository(&repo_id)
        .await
        .unwrap();
    assert!(
        !hashes.iter().any(|h| h.file_path() == "keep.rs"),
        "deleted file's hash record must be removed"
    );
}

#[tokio::test]
async fn file_count_includes_unreadable_files() {
    let (container, _data) = test_container().await;
    let repo = make_repo();
    let repo_path = repo.path().to_string_lossy().to_string();

    let indexed = container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("initial index failed");
    let file_count_before = indexed.file_count();
    assert_eq!(file_count_before, 2, "precondition: both files indexed");

    make_unreadable(&repo.path().join("keep.rs"));

    let reindexed = container
        .index_use_case()
        .execute(&repo_path, None, VectorStore::InMemory, None, false)
        .await
        .expect("incremental index failed");

    assert_eq!(
        reindexed.file_count(),
        file_count_before,
        "an unreadable file keeps its index entry, so it must keep being counted"
    );
}
