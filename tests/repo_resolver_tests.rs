//! End-to-end tests for git-remote-based namespace resolution.
//!
//! These exercise the path from "a repository was indexed under namespace X with
//! embedding config Y" to "running a command from inside that repository
//! resolves X and Y automatically" — keyed by git remote, with a canonical-path
//! fallback.
//!
//! The `repositories` and `namespace_config` tables are seeded with a plain
//! DuckDB connection (matching the production schema) so the test exercises the
//! read-only resolver without pulling in the `vss`/`fts` extensions that the
//! full vector repository loads.

use std::fs;
use std::path::Path;

use codesearch::resolve_repo_context;
use duckdb::{params, Connection};

/// Seed a fresh DuckDB file with one repository row (and matching namespace
/// config), then drop the connection so the read-only resolver can open it.
#[allow(clippy::too_many_arguments)]
fn seed_db(
    db_path: &Path,
    namespace: &str,
    repo_path: &str,
    git_remote: Option<&str>,
    embedding_target: &str,
    embedding_model: &str,
    dimensions: i64,
) {
    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE repositories (
            id TEXT PRIMARY KEY, name TEXT, path TEXT, created_at BIGINT, updated_at BIGINT,
            chunk_count BIGINT, file_count BIGINT, store TEXT, namespace TEXT,
            git_remote TEXT, languages TEXT
        );
        CREATE TABLE namespace_config (
            namespace TEXT PRIMARY KEY, embedding_target TEXT, embedding_model TEXT,
            dimensions INTEGER
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repositories
            (id, name, path, created_at, updated_at, chunk_count, file_count, store, namespace, git_remote, languages)
         VALUES (?1, 'demo', ?2, 1, 1, 1, 1, 'duckdb', ?3, ?4, NULL)",
        params!["id-1", repo_path, namespace, git_remote],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO namespace_config VALUES (?1, ?2, ?3, ?4)",
        params![namespace, embedding_target, embedding_model, dimensions],
    )
    .unwrap();
    // `conn` drops here, releasing the database lock.
}

fn write_git_remote(repo_dir: &Path, url: &str) {
    let git = repo_dir.join(".git");
    fs::create_dir_all(&git).unwrap();
    fs::write(
        git.join("config"),
        format!("[remote \"origin\"]\n\turl = {url}\n"),
    )
    .unwrap();
}

#[test]
fn resolves_namespace_and_embedding_config_by_git_remote() {
    let data_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let db_path = data_dir.path().join("codesearch.duckdb");

    let canonical = fs::canonicalize(repo_dir.path()).unwrap();
    seed_db(
        &db_path,
        "my-namespace",
        &canonical.to_string_lossy(),
        Some("github.com/owner/demo"),
        "onnx",
        "sentence-transformers/all-MiniLM-L6-v2",
        384,
    );

    // A clone at a *different* path still resolves via the git remote.
    let clone_dir = tempfile::tempdir().unwrap();
    write_git_remote(clone_dir.path(), "git@github.com:owner/demo.git");

    let ctx = resolve_repo_context(&db_path, clone_dir.path()).expect("should resolve");
    assert_eq!(ctx.namespace, "my-namespace");
    assert_eq!(ctx.matched_by, "git remote");
    assert_eq!(ctx.embedding_target.as_deref(), Some("onnx"));
    assert_eq!(ctx.embedding_dimensions, Some(384));
}

#[test]
fn resolves_namespace_by_path_when_no_remote() {
    let data_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let db_path = data_dir.path().join("codesearch.duckdb");

    let canonical = fs::canonicalize(repo_dir.path()).unwrap();
    seed_db(
        &db_path,
        "path-namespace",
        &canonical.to_string_lossy(),
        None,
        "onnx",
        "model",
        768,
    );

    // No .git in repo_dir → falls back to canonical-path matching.
    let ctx = resolve_repo_context(&db_path, repo_dir.path()).expect("should resolve by path");
    assert_eq!(ctx.namespace, "path-namespace");
    assert_eq!(ctx.matched_by, "path");
    assert_eq!(ctx.embedding_dimensions, Some(768));
}

#[test]
fn returns_none_for_unknown_repository() {
    let data_dir = tempfile::tempdir().unwrap();
    let indexed = tempfile::tempdir().unwrap();
    let db_path = data_dir.path().join("codesearch.duckdb");

    let canonical = fs::canonicalize(indexed.path()).unwrap();
    seed_db(
        &db_path,
        "ns",
        &canonical.to_string_lossy(),
        Some("github.com/owner/indexed"),
        "onnx",
        "model",
        384,
    );

    // A different, unindexed repository with an unrelated remote.
    let other = tempfile::tempdir().unwrap();
    write_git_remote(other.path(), "https://github.com/someone/else.git");
    assert!(resolve_repo_context(&db_path, other.path()).is_none());
}

#[test]
fn returns_none_when_database_missing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.duckdb");
    assert!(resolve_repo_context(&missing, dir.path()).is_none());
}
