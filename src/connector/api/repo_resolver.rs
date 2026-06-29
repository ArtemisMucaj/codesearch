//! Auto-resolution of a repository's indexing context from the global metadata.
//!
//! The `repositories` and `namespace_config` tables are global (one per DuckDB
//! file, not namespace-scoped), so a single read-only query can map a working
//! directory to the namespace it was indexed under — and to the embedding
//! configuration that namespace requires — without the caller having to know
//! either up front.
//!
//! Matching prefers the git remote (a stable, clone-independent key) and falls
//! back to the canonical on-disk path.

use std::path::{Path, PathBuf};

use duckdb::{params, AccessMode, Config, Connection};
use tracing::debug;

use crate::application::git_remote::detect_remote;

/// The indexing context resolved for a working directory.
#[derive(Debug, Clone)]
pub struct ResolvedContext {
    /// Namespace the repository was indexed under.
    pub namespace: String,
    /// Embedding backend recorded for that namespace (`"onnx"` / `"api"`).
    pub embedding_target: Option<String>,
    /// Embedding model recorded for that namespace.
    pub embedding_model: Option<String>,
    /// Embedding dimensionality recorded for that namespace.
    pub embedding_dimensions: Option<usize>,
    /// Human-readable repository name (for log messages).
    pub repository_name: String,
    /// How the repository was matched: `"git remote"` or `"path"`.
    pub matched_by: &'static str,
}

/// Resolve the indexing context for `repo_root` from the database at `db_path`.
///
/// Returns `None` when the database does not exist, the repository has not been
/// indexed, or it carries no namespace. All failures degrade to `None` so the
/// caller can simply fall back to defaults — resolution is a convenience, never
/// a hard requirement.
pub fn resolve(db_path: &Path, repo_root: &Path) -> Option<ResolvedContext> {
    if !db_path.exists() {
        return None;
    }

    let conn = match open_read_only(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            debug!("repo resolver: could not open {}: {}", db_path.display(), e);
            return None;
        }
    };

    // Prefer the git remote; fall back to the canonical path.
    let remote = detect_remote(repo_root);
    let (namespace, repository_name, matched_by) = remote
        .as_deref()
        .and_then(|r| find_by_remote(&conn, r).map(|(ns, name)| (ns, name, "git remote")))
        .or_else(|| {
            canonical(repo_root)
                .and_then(|p| find_by_path(&conn, &p))
                .map(|(ns, name)| (ns, name, "path"))
        })?;

    let (embedding_target, embedding_model, embedding_dimensions) =
        find_namespace_config(&conn, &namespace).unwrap_or((None, None, None));

    Some(ResolvedContext {
        namespace,
        embedding_target,
        embedding_model,
        embedding_dimensions,
        repository_name,
        matched_by,
    })
}

fn open_read_only(db_path: &Path) -> Result<Connection, duckdb::Error> {
    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    Connection::open_with_flags(db_path, config)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Find the namespace + name for a repository by its normalised git remote.
/// When several repositories share a remote, the most recently updated wins.
fn find_by_remote(conn: &Connection, remote: &str) -> Option<(String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name FROM repositories \
         WHERE git_remote = ?1 AND namespace IS NOT NULL \
         ORDER BY updated_at DESC LIMIT 1",
        remote,
    )
}

/// Find the namespace + name for a repository by its canonical path.
fn find_by_path(conn: &Connection, path: &Path) -> Option<(String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name FROM repositories \
         WHERE path = ?1 AND namespace IS NOT NULL LIMIT 1",
        &path.to_string_lossy(),
    )
}

fn query_repo(conn: &Connection, sql: &str, key: &str) -> Option<(String, String)> {
    let mut stmt = conn.prepare(sql).ok()?;
    stmt.query_row(params![key], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .ok()
}

#[allow(clippy::type_complexity)]
fn find_namespace_config(
    conn: &Connection,
    namespace: &str,
) -> Option<(Option<String>, Option<String>, Option<usize>)> {
    let mut stmt = conn
        .prepare(
            "SELECT embedding_target, embedding_model, dimensions \
             FROM namespace_config WHERE namespace = ?1",
        )
        .ok()?;
    stmt.query_row(params![namespace], |row| {
        let target: String = row.get(0)?;
        let model: String = row.get(1)?;
        let dims: i64 = row.get(2)?;
        Ok((Some(target), Some(model), Some(dims as usize)))
    })
    .ok()
}
