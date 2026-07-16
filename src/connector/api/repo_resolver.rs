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
use std::thread::sleep;
use std::time::Duration;

use duckdb::{params, AccessMode, Config, Connection};
use tracing::debug;

use crate::application::git_remote::detect_remote;
use crate::connector::adapter::NamespaceEmbeddingConfig;
use crate::connector::api::container::is_lock_conflict;

/// Maximum number of retry attempts when the read-only open is blocked by a
/// concurrent writer (e.g. an in-progress `codesearch index`). Mirrors the
/// retry budget used when the container itself opens the database read-only, so
/// resolution lands on the right namespace instead of silently falling back to
/// the default once the writer releases the lock.
const LOCK_RETRIES: u32 = 5;

/// Initial backoff for lock-conflict retries; doubles each attempt.
const LOCK_RETRY_INITIAL_MS: u64 = 500;

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
    /// UUID primary key of the repository row.
    pub repository_id: String,
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

    let conn = open_read_only_with_retry(db_path)?;

    // Prefer the git remote; fall back to the canonical path.
    let remote = detect_remote(repo_root);
    let (namespace, repository_name, repository_id, matched_by) = remote
        .as_deref()
        .and_then(|r| find_by_remote(&conn, r).map(|(ns, name, id)| (ns, name, id, "git remote")))
        .or_else(|| {
            canonical(repo_root)
                .and_then(|p| find_by_path(&conn, &p))
                .map(|(ns, name, id)| (ns, name, id, "path"))
        })?;

    let (embedding_target, embedding_model, embedding_dimensions) =
        find_namespace_config(&conn, &namespace).unwrap_or((None, None, None));

    Some(ResolvedContext {
        namespace,
        embedding_target,
        embedding_model,
        embedding_dimensions,
        repository_name,
        repository_id,
        matched_by,
    })
}

/// Read the stored embedding configuration for `namespace`, if it exists.
///
/// This is the source of truth written by `codesearch create` (or by the
/// first index run into an uncreated namespace); commands consult it so the
/// embedding setup never has to be re-specified on the command line.  All
/// failures degrade to `None` and the caller falls back to defaults.
pub fn namespace_embedding_config(
    db_path: &Path,
    namespace: &str,
) -> Option<NamespaceEmbeddingConfig> {
    if !db_path.exists() {
        return None;
    }
    let conn = open_read_only_with_retry(db_path)?;
    let schema = namespace.trim();
    let schema_name = if schema.is_empty() { "main" } else { schema };
    match find_namespace_config(&conn, schema_name)? {
        (Some(embedding_target), Some(embedding_model), Some(dimensions)) => {
            Some(NamespaceEmbeddingConfig {
                embedding_target,
                embedding_model,
                dimensions,
            })
        }
        _ => None,
    }
}

fn open_read_only(db_path: &Path) -> Result<Connection, duckdb::Error> {
    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    Connection::open_with_flags(db_path, config)
}

/// Open the database read-only, retrying with exponential backoff while a
/// concurrent writer holds the lock. Non-lock failures (e.g. the database does
/// not exist yet) return `None` immediately. A persistent lock after all
/// retries also yields `None` — the container open that follows runs its own
/// retry and will surface a clear error to the user.
fn open_read_only_with_retry(db_path: &Path) -> Option<Connection> {
    let mut delay_ms = LOCK_RETRY_INITIAL_MS;
    for attempt in 0..=LOCK_RETRIES {
        match open_read_only(db_path) {
            Ok(conn) => return Some(conn),
            Err(e) if attempt < LOCK_RETRIES && is_lock_conflict(&e.to_string()) => {
                if attempt == 0 {
                    debug!("repo resolver: database locked by another process; waiting…");
                }
                sleep(Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
            Err(e) => {
                debug!("repo resolver: could not open {}: {}", db_path.display(), e);
                return None;
            }
        }
    }
    None
}

fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Find the namespace + name + id for a repository by its normalised git remote.
/// When several repositories share a remote, the most recently updated wins.
fn find_by_remote(conn: &Connection, remote: &str) -> Option<(String, String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name, id FROM repositories \
         WHERE git_remote = ?1 AND namespace IS NOT NULL \
         ORDER BY updated_at DESC LIMIT 1",
        remote,
    )
}

/// Find the namespace + name + id for a repository by its canonical path.
fn find_by_path(conn: &Connection, path: &Path) -> Option<(String, String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name, id FROM repositories \
         WHERE path = ?1 AND namespace IS NOT NULL LIMIT 1",
        &path.to_string_lossy(),
    )
}

fn query_repo(conn: &Connection, sql: &str, key: &str) -> Option<(String, String, String)> {
    let mut stmt = conn.prepare(sql).ok()?;
    stmt.query_row(params![key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })
    .ok()
}

/// Resolve the memory scope for a working directory.
///
/// Repositories the user deliberately indexed together under a named
/// namespace are correlated — they work together — so their sessions share
/// one memory scope: the namespace. A directory that is not indexed (or was
/// indexed into the catch-all default namespace) gets a per-project scope:
/// its directory name.
///
/// Returns `None` only for an empty/root-only path. All resolution failures
/// (missing database, lock timeouts) degrade to the per-project fallback.
pub fn resolve_memory_scope(db_path: &Path, cwd: &str) -> Option<String> {
    if let Some(ctx) = resolve(db_path, Path::new(cwd)) {
        if ctx.namespace != crate::cli::DEFAULT_NAMESPACE {
            debug!(
                "memory scope for '{}' resolved to namespace '{}' (matched by {})",
                cwd, ctx.namespace, ctx.matched_by
            );
            return Some(ctx.namespace);
        }
    }
    crate::connector::adapter::project_from_cwd(cwd)
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
